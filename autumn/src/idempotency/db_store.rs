//! Database idempotency store (issue #3061).
//!
//! Keeps records in the app database, in `autumn_idempotency_keys`. A handler
//! can then write its response in its own `Db::tx`, through
//! [`IdempotencyTx::commit`]. The response commits with the mutation, or
//! neither commits.
//!
//! The lock row carries its owner. The in-transaction write checks that owner,
//! so a request whose lock expired cannot commit: its transaction rolls back.
//!
//! Times are Unix milliseconds from the app clock, so a `Sim` moves them.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::http::Response;
use axum::response::IntoResponse;
use diesel::{
    BoolExpressionMethods as _, ExpressionMethods as _, OptionalExtension as _, QueryDsl as _,
};
use diesel_async::RunQueryDsl as _;
use diesel_async::pooled_connection::deadpool::{Object, Pool};

use super::{
    IdempotencyEntry, IdempotencyFuture, IdempotencyRecord, IdempotencyReplayMetadata,
    IdempotencyStore, IdempotencyStoreError, MAX_CACHEABLE_RESPONSE_BODY, StoredEntry,
    cacheable_response_record,
};
use crate::db::RuntimeConnection;
use crate::error::{AutumnError, AutumnResult};
use crate::session::Session;

diesel::table! {
    autumn_idempotency_keys (storage_key) {
        storage_key -> Text,
        record -> Nullable<Binary>,
        recovery_point -> Nullable<Text>,
        recovery_body_hash -> Nullable<Binary>,
        locked_by -> Nullable<Text>,
        locked_until_ms -> BigInt,
        expires_at_ms -> BigInt,
        ttl_ms -> BigInt,
        record_owner -> Nullable<Text>,
    }
}

use autumn_idempotency_keys::dsl as keys;

/// Delete expired rows once per this many writes.
const SWEEP_EVERY: u64 = 128;

fn now_ms() -> i64 {
    crate::time::ambient_now().timestamp_millis()
}

/// The longest span the store writes, in milliseconds: about 18 million
/// years. The expiry arithmetic adds a few of these to the clock in SQL, so
/// a longer TTL is clamped here rather than overflowing `BIGINT` there.
const MAX_SPAN_MS: i64 = i64::MAX / 16;

fn ms(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).map_or(MAX_SPAN_MS, |ms| ms.min(MAX_SPAN_MS))
}

fn after(duration: Duration) -> i64 {
    now_ms().saturating_add(ms(duration))
}

fn db_error(action: &str, error: impl std::fmt::Display) -> IdempotencyStoreError {
    IdempotencyStoreError::backend(format!("failed to {action} in the database: {error}"))
}

/// Idempotency store in the app database (Postgres, or `SQLite` under the
/// `sqlite` feature).
///
/// Use it with [`IdempotencyTx`] to commit the response in the handler's
/// transaction. Without `IdempotencyTx`, it works like the other stores: the
/// middleware writes the response after the handler returns.
///
/// Needs the framework migration that creates `autumn_idempotency_keys`.
/// Select it with `backend = "database"` in `[idempotency]`.
///
/// A GDPR legal hold on `autumn_idempotency_keys` (a
/// `ModelRegistration::retain`) stops the store from deleting or
/// overwriting an expired row. See [`Self::with_legal_holds`].
pub struct DbIdempotencyStore {
    pool: Pool<RuntimeConnection>,
    default_ttl: Duration,
    writes: AtomicU64,
    /// Where the [`crate::gdpr::GdprRegistry`] is read from, for legal holds.
    state: Option<crate::AppState>,
}

/// The store's table, as a GDPR registration names it.
const TABLE: &str = "autumn_idempotency_keys";

impl DbIdempotencyStore {
    /// Create a store on `pool`. `default_ttl` is the response retention.
    #[must_use]
    pub const fn new(pool: Pool<RuntimeConnection>, default_ttl: Duration) -> Self {
        Self {
            pool,
            default_ttl,
            writes: AtomicU64::new(0),
            state: None,
        }
    }

    /// Honour GDPR legal holds from the [`crate::gdpr::GdprRegistry`] in
    /// `state`. The router sets this for `backend = "database"`.
    ///
    /// While `autumn_idempotency_keys` is registered with
    /// `ModelRegistration::retain`, expired rows stay: the expiry sweep does
    /// not run, and an expired response or recovery point is never deleted
    /// or overwritten. A request that reuses such a key gets `409`.
    #[must_use]
    pub fn with_legal_holds(mut self, state: &crate::AppState) -> Self {
        self.state = Some(state.clone());
        self
    }

    /// The reason the table is under a legal hold, if it is. Read on each
    /// call, as the job-tracking cleanup does: the registry is app state.
    fn legal_hold(&self) -> Option<String> {
        let registry = self
            .state
            .as_ref()?
            .extension::<crate::gdpr::GdprRegistry>();
        crate::data_retention::table_legal_hold(TABLE, registry.as_deref())
    }

    /// The expiry before which an expired row may be deleted or overwritten:
    /// `now`, or never while the table is under a legal hold.
    fn reclaim_before(&self, now: i64) -> i64 {
        if self.legal_hold().is_some() {
            i64::MIN
        } else {
            now
        }
    }

    async fn conn(&self) -> Result<Object<RuntimeConnection>, IdempotencyStoreError> {
        self.pool
            .get()
            .await
            .map_err(|e| db_error("check out a connection", e))
    }

    /// Delete expired rows, once per [`SWEEP_EVERY`] writes. A failure is
    /// logged only: the next sweep tries again.
    async fn sweep(&self, conn: &mut RuntimeConnection) {
        if !self
            .writes
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(SWEEP_EVERY)
        {
            return;
        }
        if let Some(reason) = self.legal_hold() {
            tracing::debug!(
                reason = %reason,
                "idempotency key sweep skipped: autumn_idempotency_keys is under legal hold"
            );
            return;
        }
        let now = now_ms();
        // A row with a live lock stays: its owner may still commit to it.
        let expired = keys::autumn_idempotency_keys
            .filter(keys::expires_at_ms.le(now))
            .filter(keys::locked_until_ms.le(now));
        if let Err(error) = diesel::delete(expired).execute(conn).await {
            tracing::warn!(%error, "Idempotency key sweep failed; the next sweep retries");
        }
    }
}

impl DbIdempotencyStore {
    /// Keep `owner`'s lock until the record expires.
    ///
    /// The middleware calls this before a session rewrite of a record that
    /// committed in the handler's transaction. That record has no final
    /// `Set-Cookie` yet. If the rewrite never ends (a crash, a failed save),
    /// the key stays busy and the record is never replayed without its cookie.
    /// A successful rewrite releases the lock.
    pub(super) async fn hold_until_expiry(
        &self,
        key: &str,
        owner: &str,
    ) -> Result<(), IdempotencyStoreError> {
        let mut conn = self.conn().await?;
        extend_lock_to_expiry(&mut conn, key, owner)
            .await
            .map_err(|e| db_error("hold idempotency lock", e))
    }
}

impl DbIdempotencyStore {
    /// `true` when `owner` still holds `key` and its row has a record.
    async fn holds_record(&self, key: &str, owner: &str) -> Result<bool, IdempotencyStoreError> {
        let mut conn = self.conn().await?;
        keys::autumn_idempotency_keys
            .filter(keys::storage_key.eq(key))
            .filter(keys::locked_by.eq(owner))
            .filter(keys::record.is_not_null())
            .select(keys::storage_key)
            .first::<String>(&mut conn)
            .await
            .optional()
            .map(|row| row.is_some())
            .map_err(|e| db_error("read idempotency record", e))
    }

    /// Copy the recovery point of the row `from_owner` holds on `from` to the
    /// row `to_owner` holds on `to`, with its body hash and TTL, unless that
    /// row has one already. The copy lives its TTL from now, and at least one
    /// TTL past `to`'s lock, as a recovery point set under it does.
    ///
    /// The middleware calls this for a retry whose session cookie went stale:
    /// the retry runs under its new session's key, and must still see the
    /// steps the first attempt committed under the old one.
    ///
    /// `false` when `from_owner` no longer holds `from`: its lock expired and
    /// another request took the key. The old key may still hold a point this
    /// call cannot read, so the caller must not run the handler. `false` too
    /// when `to_owner` no longer holds `to`.
    pub(super) async fn adopt_recovery_point(
        &self,
        from: &str,
        from_owner: &str,
        to: &str,
        to_owner: &str,
    ) -> Result<bool, IdempotencyStoreError> {
        let mut conn = self.conn().await?;
        let held: Option<(Option<String>, Option<Vec<u8>>, i64)> = keys::autumn_idempotency_keys
            .filter(keys::storage_key.eq(from))
            .filter(keys::locked_by.eq(from_owner))
            .select((keys::recovery_point, keys::recovery_body_hash, keys::ttl_ms))
            .first(&mut conn)
            .await
            .optional()
            .map_err(|e| db_error("read idempotency recovery point", e))?;
        let Some((point, body_hash, ttl_ms)) = held else {
            return Ok(false);
        };
        let copied = HeldPoint {
            point,
            body_hash,
            ttl_ms,
        };
        self.copy_held_point(&mut conn, from, from_owner, copied, to, to_owner)
            .await
    }

    /// Write `copied` (read from `from`) to the row `to_owner` holds on `to`,
    /// only while `from_owner` still holds `from` with that point (or with
    /// none, when `copied` has none). `false` when it no longer does, or when
    /// `to_owner` no longer holds `to`.
    ///
    /// One transaction. It first rewrites the source row with its own values
    /// while `from_owner` still holds it with the point. That write locks the
    /// row until the copy commits: on Postgres, a request taking the key over
    /// waits for the copy, and a takeover already in flight makes this write
    /// wait and then find the key lost. A check that only reads the source,
    /// even in the copy's own statement, sees its snapshot and does neither.
    async fn copy_held_point(
        &self,
        conn: &mut RuntimeConnection,
        from: &str,
        from_owner: &str,
        copied: HeldPoint,
        to: &str,
        to_owner: &str,
    ) -> Result<bool, IdempotencyStoreError> {
        use scoped_futures::ScopedFutureExt as _;

        let (from, from_owner, to, to_owner) = (
            from.to_owned(),
            from_owner.to_owned(),
            to.to_owned(),
            to_owner.to_owned(),
        );
        crate::db::scoped_transaction(conn, move |conn| {
            async move {
                let HeldPoint {
                    point,
                    body_hash,
                    ttl_ms,
                } = copied;
                let source = keys::autumn_idempotency_keys
                    .filter(keys::storage_key.eq(&from))
                    .filter(keys::locked_by.eq(&from_owner));
                let unchanged = keys::locked_until_ms.eq(keys::locked_until_ms);
                let fenced = match &point {
                    Some(point) => {
                        diesel::update(source.filter(keys::recovery_point.eq(point)))
                            .set(unchanged)
                            .execute(&mut *conn)
                            .await?
                    }
                    None => {
                        diesel::update(source.filter(keys::recovery_point.is_null()))
                            .set(unchanged)
                            .execute(&mut *conn)
                            .await?
                    }
                };
                if fenced == 0 {
                    return Ok(false);
                }
                let target = keys::autumn_idempotency_keys
                    .filter(keys::storage_key.eq(&to))
                    .filter(keys::locked_by.eq(&to_owner));
                if let Some(point) = point {
                    #[allow(
                        clippy::arithmetic_side_effects,
                        reason = "a SQL expression, evaluated by the database"
                    )]
                    let crash_expiry = keys::locked_until_ms + ttl_ms;
                    // The point's own TTL from now, or past the lock if that
                    // is later; not the store default the lock set.
                    let fresh_expiry = now_ms().saturating_add(ttl_ms);
                    let expiry =
                        diesel::dsl::case_when(crash_expiry.gt(fresh_expiry), crash_expiry)
                            .otherwise(fresh_expiry);
                    let copied = diesel::update(target.filter(keys::recovery_point.is_null()))
                        .set((
                            keys::recovery_point.eq(Some(point)),
                            keys::recovery_body_hash.eq(body_hash),
                            keys::ttl_ms.eq(ttl_ms),
                            keys::expires_at_ms.eq(expiry),
                        ))
                        .execute(&mut *conn)
                        .await?;
                    if copied == 1 {
                        return Ok(true);
                    }
                }
                // Nothing copied: there was no point, the target has one
                // already, or it lost its lock to another request. Go on only
                // while the target is held.
                let target_held = target
                    .select(keys::storage_key)
                    .first::<String>(&mut *conn)
                    .await
                    .optional()?
                    .is_some();
                Ok::<_, diesel::result::Error>(target_held)
            }
            .scope_boxed()
        })
        .await
        .map_err(|e| db_error("copy idempotency recovery point", e))
    }
}

/// A recovery point read from one key, to copy to another.
struct HeldPoint {
    /// `None` when the source has no point: nothing to copy, but both locks
    /// are still checked.
    point: Option<String>,
    body_hash: Option<Vec<u8>>,
    ttl_ms: i64,
}

/// Keep `owner`'s lock on `key` until the record expires. A lock that already
/// ends later does not change.
async fn extend_lock_to_expiry(
    conn: &mut RuntimeConnection,
    key: &str,
    owner: &str,
) -> diesel::QueryResult<()> {
    let held = keys::autumn_idempotency_keys
        .filter(keys::storage_key.eq(key))
        .filter(keys::locked_by.eq(owner))
        .filter(keys::expires_at_ms.gt(keys::locked_until_ms));
    diesel::update(held)
        .set(keys::locked_until_ms.eq(keys::expires_at_ms))
        .execute(conn)
        .await
        .map(drop)
}

impl IdempotencyStore for DbIdempotencyStore {
    fn get<'a>(&'a self, key: &'a str) -> IdempotencyFuture<'a, Option<IdempotencyEntry>> {
        Box::pin(async move {
            let mut conn = self.conn().await?;
            let now = now_ms();
            // A record is not replayable while its owner still holds the lock:
            // the owner may still rewrite it (a session change) or fail.
            let record: Option<Option<Vec<u8>>> = keys::autumn_idempotency_keys
                .filter(keys::storage_key.eq(key))
                .filter(keys::expires_at_ms.gt(now))
                .filter(keys::locked_by.is_null().or(keys::locked_until_ms.le(now)))
                .select(keys::record)
                .first(&mut conn)
                .await
                .optional()
                .map_err(|e| db_error("read idempotency key", e))?;
            record
                .flatten()
                .map(|bytes: Vec<u8>| StoredEntry::decode(&bytes))
                .transpose()
        })
    }

    fn set<'a>(
        &'a self,
        key: &'a str,
        owner: &'a str,
        record: IdempotencyRecord,
        body_hash: Vec<u8>,
        ttl: Duration,
    ) -> IdempotencyFuture<'a, ()> {
        Box::pin(async move {
            let bytes = StoredEntry::encode(record, body_hash)?;
            // The clock after checkout: a slow pool must not eat the TTL.
            let mut conn = self.conn().await?;
            let now = now_ms();
            let ttl_ms = ms(ttl);
            let expires = now.saturating_add(ttl_ms);
            // An existing row keeps its lock: it may be a newer owner's, and
            // the caller releases its own with `unlock`. While a lock is live,
            // the record stays one TTL past it, so a crash before `unlock`
            // still leaves a record to replay; `unlock` then resets the
            // expiry to `ttl_ms` from the release.
            #[allow(
                clippy::arithmetic_side_effects,
                reason = "a SQL expression, evaluated by the database"
            )]
            let held_expiry = keys::locked_until_ms + ttl_ms;
            let expiry = diesel::dsl::case_when(keys::locked_until_ms.gt(now), held_expiry)
                .otherwise(expires);
            let upsert = diesel::insert_into(keys::autumn_idempotency_keys)
                .values((
                    keys::storage_key.eq(key),
                    keys::record.eq(Some(&bytes)),
                    keys::locked_by.eq(None::<String>),
                    keys::locked_until_ms.eq(0),
                    keys::expires_at_ms.eq(expires),
                    keys::ttl_ms.eq(ttl_ms),
                    keys::record_owner.eq(owner),
                ))
                .on_conflict(keys::storage_key)
                .do_update()
                .set((
                    keys::record.eq(Some(&bytes)),
                    keys::expires_at_ms.eq(expiry),
                    keys::ttl_ms.eq(ttl_ms),
                    keys::record_owner.eq(owner),
                ));
            // Write nothing when another owner holds a live lock, or stored a
            // record that has not expired: a request that outlived its lock
            // never replaces the newer response.
            // `ON CONFLICT … DO UPDATE … WHERE`: the `WHERE` reads the existing row.
            let lock_free = keys::locked_until_ms
                .le(now)
                .or(keys::locked_by.is_null())
                .or(keys::locked_by.eq(owner));
            // An expired record is free too, unless a legal hold keeps it.
            let record_free = keys::record
                .is_null()
                .or(keys::expires_at_ms.le(self.reclaim_before(now)))
                .or(keys::record_owner.eq(owner));
            diesel::query_dsl::methods::FilterDsl::filter(upsert, lock_free.and(record_free))
                .execute(&mut conn)
                .await
                .map_err(|e| db_error("store idempotency record", e))?;
            self.sweep(&mut conn).await;
            Ok(())
        })
    }

    fn try_lock<'a>(
        &'a self,
        key: &'a str,
        owner: &'a str,
        lock_ttl: Duration,
    ) -> IdempotencyFuture<'a, bool> {
        Box::pin(async move {
            let mut conn = self.conn().await?;
            self.sweep(&mut conn).await;
            // The clock after checkout and the sweep: a slow pool must not
            // eat the lock's lifetime.
            let now = now_ms();
            let lock_ttl = if lock_ttl.is_zero() {
                Duration::from_secs(1)
            } else {
                lock_ttl
            };
            let until = now.saturating_add(ms(lock_ttl));
            let expires = until.max(after(self.default_ttl));
            // Check first without a row lock. On Postgres the upsert below
            // waits for any open transaction that changed the row, so a
            // duplicate of a running request must return here, at once.
            // Busy: a live lock, or a record that has not expired.
            let busy = keys::autumn_idempotency_keys
                .filter(keys::storage_key.eq(key))
                .filter(
                    keys::locked_until_ms
                        .gt(now)
                        .or(keys::record.is_not_null().and(keys::expires_at_ms.gt(now))),
                )
                .select(keys::storage_key)
                .first::<String>(&mut conn)
                .await
                .optional()
                .map_err(|e| db_error("read idempotency lock", e))?
                .is_some();
            if busy {
                return Ok(false);
            }
            // An expired key with no live lock starts over, unless a legal
            // hold keeps it: then the upsert below refuses a row that holds
            // an expired response or recovery point.
            let reclaim_before = self.reclaim_before(now);
            diesel::delete(
                keys::autumn_idempotency_keys
                    .filter(keys::storage_key.eq(key))
                    .filter(keys::expires_at_ms.le(reclaim_before))
                    .filter(keys::locked_until_ms.le(now)),
            )
            .execute(&mut conn)
            .await
            .map_err(|e| db_error("delete expired idempotency key", e))?;
            // Take the lock when the key is new, or has no record and its lock
            // expired. A recovery point stays for the next owner: the row
            // lives one TTL past the new lock, or longer if it already did,
            // so a crash of this owner too leaves it for the one after.
            // The row's own TTL (the layer's, stored with a recovery point
            // or record); the store default when the row has none.
            let default_ttl = ms(self.default_ttl);
            #[allow(
                clippy::arithmetic_side_effects,
                reason = "a SQL expression, evaluated by the database"
            )]
            let crash_expires = || {
                diesel::dsl::case_when(keys::ttl_ms.gt(0), keys::ttl_ms + until)
                    .otherwise(default_ttl.saturating_add(until))
            };
            let taken_expiry = diesel::dsl::case_when(
                keys::expires_at_ms.gt(crash_expires()),
                keys::expires_at_ms,
            )
            .otherwise(crash_expires());
            let upsert = diesel::insert_into(keys::autumn_idempotency_keys)
                .values((
                    keys::storage_key.eq(key),
                    keys::locked_by.eq(owner),
                    keys::locked_until_ms.eq(until),
                    keys::expires_at_ms.eq(expires),
                ))
                .on_conflict(keys::storage_key)
                .do_update()
                .set((
                    keys::locked_by.eq(owner),
                    keys::locked_until_ms.eq(until),
                    keys::expires_at_ms.eq(taken_expiry),
                ));
            // `ON CONFLICT … DO UPDATE … WHERE`: the `WHERE` reads the existing row.
            let kept = keys::recovery_point
                .is_not_null()
                .and(keys::expires_at_ms.le(now))
                .and(keys::expires_at_ms.gt(reclaim_before));
            let acquired = diesel::query_dsl::methods::FilterDsl::filter(
                upsert,
                keys::record
                    .is_null()
                    .and(keys::locked_until_ms.le(now))
                    .and(diesel::dsl::not(kept)),
            )
            .execute(&mut conn)
            .await
            .map_err(|e| db_error("acquire idempotency lock", e))?;
            Ok(acquired == 1)
        })
    }

    fn unlock<'a>(&'a self, key: &'a str, owner: &'a str) -> IdempotencyFuture<'a, ()> {
        Box::pin(async move {
            let mut conn = self.conn().await?;
            let held = keys::autumn_idempotency_keys
                .filter(keys::storage_key.eq(key))
                .filter(keys::locked_by.eq(owner));
            // A key with no record and no recovery point keeps nothing.
            diesel::delete(
                held.filter(keys::record.is_null())
                    .filter(keys::recovery_point.is_null()),
            )
            .execute(&mut conn)
            .await
            .map_err(|e| db_error("release idempotency lock", e))?;
            // What is left holds a record or a recovery point, and lives
            // `ttl_ms` from now (a zero TTL included): every writer of either
            // stores `ttl_ms`. One statement: if the release stops before it,
            // the row keeps its crash-safe expiry.
            #[allow(
                clippy::arithmetic_side_effects,
                reason = "a SQL expression, evaluated by the database"
            )]
            let release_expiry = keys::ttl_ms + now_ms();
            diesel::update(held)
                .set((
                    keys::expires_at_ms.eq(release_expiry),
                    keys::locked_by.eq(None::<String>),
                    keys::locked_until_ms.eq(0),
                ))
                .execute(&mut conn)
                .await
                .map(drop)
                .map_err(|e| db_error("release idempotency lock", e))
        })
    }

    fn default_ttl(&self) -> Duration {
        self.default_ttl
    }
}

/// The request's claim on its idempotency key, for use in a `Db::tx`.
struct TxClaim {
    storage_key: String,
    owner: String,
    body_hash: Vec<u8>,
    ttl: Duration,
    committed: AtomicBool,
    /// An upper bound on this request's lock deadline (Unix ms, app clock):
    /// read just after the store set the lock, and moved when the lock moves.
    /// Before it, a missing row means another database (`500`); after it, the
    /// sweep may have deleted the row, so the key was lost (`409`). The two
    /// overlap only between the store's deadline and this bound: the time it
    /// takes the middleware to read the clock after the lock statement.
    lock_deadline_ms: AtomicI64,
}

/// Extractor: write the idempotency record in the handler's transaction.
///
/// Active only when the request has an `Idempotency-Key` and the app uses
/// [`DbIdempotencyStore`]. When inactive, every method is a no-op, so the same
/// handler runs with any store.
///
/// ```rust,ignore
/// #[post("/pay")]
/// async fn pay(idem: IdempotencyTx, mut db: Db) -> AutumnResult<Response> {
///     db.tx(|conn| async move {
///         let id = insert_payment(conn).await?;
///         idem.commit(conn, (StatusCode::CREATED, Json(id))).await
///     }.scope_boxed()).await
/// }
/// ```
///
/// Return the response that `commit` gives back. A retry replays exactly that
/// response.
///
/// The middleware finds the store by its exact type. A wrapper around
/// [`DbIdempotencyStore`] makes this extractor inactive.
#[derive(Clone, Default)]
pub struct IdempotencyTx {
    claim: Option<Arc<TxClaim>>,
    session: Option<Session>,
}

impl IdempotencyTx {
    pub(super) fn new(
        storage_key: String,
        owner: String,
        body_hash: Vec<u8>,
        ttl: Duration,
        lock_deadline_ms: i64,
    ) -> Self {
        Self {
            claim: Some(Arc::new(TxClaim {
                storage_key,
                owner,
                body_hash,
                ttl,
                committed: AtomicBool::new(false),
                lock_deadline_ms: AtomicI64::new(lock_deadline_ms),
            })),
            session: None,
        }
    }

    /// `true` when the methods write to the database.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.claim.is_some()
    }

    /// `true` when [`Self::commit`] wrote the record and its transaction
    /// committed.
    ///
    /// `commit` can succeed in a transaction that then rolls back, so the
    /// record must still be in `store`. A read error gives `false`: the
    /// middleware then treats the response as it does for any other store.
    pub(super) async fn committed(&self, store: &DbIdempotencyStore) -> bool {
        let Some(claim) = &self.claim else {
            return false;
        };
        if !AtomicBool::load(&claim.committed, Ordering::SeqCst) {
            return false;
        }
        store
            .holds_record(&claim.storage_key, &claim.owner)
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "Idempotency record check failed; treating it as not committed");
                false
            })
    }

    /// Store `response` as this key's record, in the transaction of `conn`.
    ///
    /// Returns the response to send. When inactive, returns `response`
    /// unchanged.
    ///
    /// # Errors
    ///
    /// - `409` when this request no longer holds the key (its in-flight lock
    ///   expired and another request took it). The transaction must roll back.
    /// - `500` when `conn` is not a primary database connection (for example,
    ///   a shard). The key row is only on the primary database.
    /// - `422` when a recovery point of this key came from another request
    ///   body.
    /// - `500` when the body is larger than 10 MiB or cannot be read, or the
    ///   write fails.
    ///
    /// If the session already changed, the key stays locked until the session
    /// rewrite ends, so a retry never replays a record without its
    /// `Set-Cookie`.
    pub fn commit<'c>(
        &self,
        conn: &'c mut RuntimeConnection,
        response: impl IntoResponse,
    ) -> impl Future<Output = AutumnResult<Response<Body>>> + Send + 'c {
        let response = response.into_response();
        let claim = self.claim.clone();
        let session = self.session.clone();
        async move {
            let Some(claim) = claim else {
                return Ok(response);
            };
            held_row(conn, &claim).await?;
            let (parts, body) = response.into_parts();
            let bytes = axum::body::to_bytes(body, MAX_CACHEABLE_RESPONSE_BODY)
                .await
                .map_err(|e| {
                    AutumnError::internal_server_error_msg(format!(
                        "idempotent response body cannot be stored: {e}"
                    ))
                })?;
            let metadata = parts
                .extensions
                .get::<IdempotencyReplayMetadata>()
                .map(|metadata| metadata.entries.clone())
                .unwrap_or_default();
            let record =
                cacheable_response_record(parts.status.as_u16(), &parts.headers, &bytes, metadata);
            let encoded = StoredEntry::encode(record, claim.body_hash.clone())?;
            // The lock stays. The middleware releases it after the response
            // is final; after a crash, the lock TTL frees it. Until then a
            // retry gets `409`, not this record.
            let ttl = ms(claim.ttl);
            let expires = now_ms().saturating_add(ttl);
            let written = diesel::update(owned_key(&claim))
                .set((
                    keys::record.eq(Some(encoded)),
                    keys::expires_at_ms.eq(expires),
                    keys::ttl_ms.eq(ttl),
                    keys::record_owner.eq(Some(claim.owner.clone())),
                ))
                .execute(conn)
                .await?;
            if written != 1 {
                return Err(claim_error(conn, &claim).await);
            }
            // `unlock` sets the expiry back to `now + ttl` on a normal release.
            keep_past_crash_lock(conn, &claim).await?;
            // The session already changed, so this record has no final
            // `Set-Cookie`. Keep it hidden until the session rewrite ends.
            if let Some(session) = &session
                && session.has_pending_changes().await
            {
                extend_lock_to_expiry(conn, &claim.storage_key, &claim.owner).await?;
                // The lock now ends at the record expiry, which
                // `keep_past_crash_lock` may have moved to the old deadline
                // plus the TTL. Keep an upper bound on both.
                let held_until = AtomicI64::load(&claim.lock_deadline_ms, Ordering::Relaxed)
                    .saturating_add(ttl)
                    .max(expires);
                claim
                    .lock_deadline_ms
                    .fetch_max(held_until, Ordering::Relaxed);
            }
            AtomicBool::store(&claim.committed, true, Ordering::SeqCst);
            Ok(Response::from_parts(parts, Body::from(bytes)))
        }
    }

    /// Record that a step of a multi-step handler committed, in the
    /// transaction of `conn`. A retry reads it with [`Self::recovery_point`].
    ///
    /// # Errors
    ///
    /// - `409` when this request no longer holds the key.
    /// - `422` when a recovery point of this key came from another request
    ///   body.
    /// - `500` when the key row is not on `conn` (for example, a shard
    ///   connection), or the write fails.
    pub fn set_recovery_point<'c>(
        &self,
        conn: &'c mut RuntimeConnection,
        point: &str,
    ) -> impl Future<Output = AutumnResult<()>> + Send + 'c {
        let claim = self.claim.clone();
        let point = point.to_owned();
        async move {
            let Some(claim) = claim else {
                return Ok(());
            };
            held_row(conn, &claim).await?;
            let ttl = ms(claim.ttl);
            let written = diesel::update(owned_key(&claim))
                .set((
                    keys::recovery_point.eq(Some(point)),
                    keys::recovery_body_hash.eq(Some(claim.body_hash.clone())),
                    // The layer's TTL, not the store default the lock set:
                    // `keep_past_crash_lock` below only moves it later.
                    keys::expires_at_ms.eq(now_ms().saturating_add(ttl)),
                    // The layer's TTL, for a later owner that takes the row over.
                    keys::ttl_ms.eq(ttl),
                ))
                .execute(conn)
                .await?;
            if written != 1 {
                return Err(claim_error(conn, &claim).await);
            }
            keep_past_crash_lock(conn, &claim).await?;
            Ok(())
        }
    }

    /// The last recovery point of this key, or `None` if no step committed.
    /// Always `None` when inactive.
    ///
    /// # Errors
    ///
    /// - `409` when another request took the key (this request's lock expired).
    /// - `422` when the recovery point came from another request body. A step
    ///   done for one body is not done for another.
    /// - `500` when the key row is not on `conn` (for example, a shard
    ///   connection), or the read fails.
    pub fn recovery_point<'c>(
        &self,
        conn: &'c mut RuntimeConnection,
    ) -> impl Future<Output = AutumnResult<Option<String>>> + Send + 'c {
        let claim = self.claim.clone();
        async move {
            let Some(claim) = claim else {
                return Ok(None);
            };
            held_row(conn, &claim).await
        }
    }
}

/// The key row, filtered to its current lock owner.
type HeldKey<'a> = diesel::dsl::Filter<
    diesel::dsl::Filter<
        autumn_idempotency_keys::table,
        diesel::dsl::Eq<keys::storage_key, &'a str>,
    >,
    diesel::dsl::Eq<keys::locked_by, &'a str>,
>;

/// The key row, only while this request holds the lock.
fn owned_key(claim: &TxClaim) -> HeldKey<'_> {
    keys::autumn_idempotency_keys
        .filter(keys::storage_key.eq(claim.storage_key.as_str()))
        .filter(keys::locked_by.eq(claim.owner.as_str()))
}

/// Keep the held row for one TTL after its lock frees.
///
/// After a crash, a retry can take the key only when the lock frees. The row
/// (a record or a recovery point) must still be there, or the retry runs the
/// committed work again. The expiry only moves later.
async fn keep_past_crash_lock(
    conn: &mut RuntimeConnection,
    claim: &TxClaim,
) -> diesel::QueryResult<()> {
    #[allow(
        clippy::arithmetic_side_effects,
        reason = "a SQL expression, evaluated by the database"
    )]
    let crash_expiry = keys::locked_until_ms + ms(claim.ttl);
    let live = owned_key(claim)
        .filter(keys::locked_until_ms.gt(now_ms()))
        .filter(keys::expires_at_ms.lt(crash_expiry));
    diesel::update(live)
        .set(keys::expires_at_ms.eq(crash_expiry))
        .execute(conn)
        .await
        .map(drop)
}

/// The recovery point of the row this request holds.
///
/// - No such row: [`claim_error`]. An active claim always has its row, so a
///   missing row is not "no step committed".
/// - The recovery point came from another request body: `422`, as for a
///   replay with another body.
async fn held_row(conn: &mut RuntimeConnection, claim: &TxClaim) -> AutumnResult<Option<String>> {
    let row: Option<(Option<String>, Option<Vec<u8>>)> = owned_key(claim)
        .select((keys::recovery_point, keys::recovery_body_hash))
        .first(conn)
        .await
        .optional()?;
    match row {
        None => Err(claim_error(conn, claim).await),
        Some((_, Some(hash))) if hash != claim.body_hash => Err(AutumnError::unprocessable_msg(
            "idempotency key reused with different payload",
        )),
        Some((point, _)) => Ok(point),
    }
}

/// The error for a write that found no row held by this request.
///
/// - The row is on this connection: another request took the key. `409`.
/// - The row is not on this connection (for example, a shard connection):
///   the write can never work. `500`.
async fn claim_error(conn: &mut RuntimeConnection, claim: &TxClaim) -> AutumnError {
    let found = keys::autumn_idempotency_keys
        .filter(keys::storage_key.eq(claim.storage_key.as_str()))
        .select(keys::storage_key)
        .first::<String>(conn)
        .await
        .optional();
    match found {
        Ok(Some(_)) => AutumnError::conflict_msg(
            "the idempotency key is held by another request; this transaction rolls back",
        ),
        // Past the lock deadline the sweep may have deleted the row: the
        // request lost its key, as when another request took it.
        Ok(None) if now_ms() >= AtomicI64::load(&claim.lock_deadline_ms, Ordering::Relaxed) => {
            AutumnError::conflict_msg(
                "the idempotency key expired before this write; this transaction rolls back",
            )
        }
        Ok(None) => AutumnError::internal_server_error_msg(
            "the idempotency key is not on this database connection; \
             call IdempotencyTx on a primary `Db` connection, not a shard",
        ),
        Err(error) => error.into(),
    }
}

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for IdempotencyTx {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        let mut tx = parts.extensions.get::<Self>().cloned().unwrap_or_default();
        tx.session = parts.extensions.get::<Session>().cloned();
        Ok(tx)
    }
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;
    use crate::sim::substrate::SqliteSubstrate;

    /// The first owner of the old key lost its lock to a second request
    /// before the copy. The copy reports that, rather than "no recovery
    /// point", so the middleware does not run the handler.
    #[tokio::test]
    async fn adopt_reports_a_stale_lock_taken_by_another_request() {
        let substrate = SqliteSubstrate::with_migrations(&[&crate::migrate::FRAMEWORK_MIGRATIONS])
            .expect("substrate");
        let store = DbIdempotencyStore::new(substrate.pool(), Duration::from_secs(60));
        assert!(
            store
                .try_lock("old", "a1", Duration::from_millis(5))
                .await
                .unwrap()
        );
        let mut conn = store.conn().await.unwrap();
        diesel::update(keys::autumn_idempotency_keys.filter(keys::storage_key.eq("old")))
            .set((
                keys::recovery_point.eq(Some("charged")),
                keys::ttl_ms.eq(60_000),
            ))
            .execute(&mut conn)
            .await
            .unwrap();
        drop(conn);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            store
                .try_lock("old", "a2", Duration::from_secs(60))
                .await
                .unwrap(),
            "a second request takes the old key once a1's lock expired"
        );
        assert!(
            store
                .try_lock("new", "b", Duration::from_secs(60))
                .await
                .unwrap()
        );

        let adopted = store
            .adopt_recovery_point("old", "a1", "new", "b")
            .await
            .unwrap();
        assert!(!adopted, "a1 lost the old key: not a key with no point");

        let adopted = store
            .adopt_recovery_point("old", "a2", "new", "b")
            .await
            .unwrap();
        assert!(adopted, "the holder copies the point");
        let mut conn = store.conn().await.unwrap();
        let point: Option<String> = keys::autumn_idempotency_keys
            .filter(keys::storage_key.eq("new"))
            .select(keys::recovery_point)
            .first(&mut conn)
            .await
            .unwrap();
        assert_eq!(point.as_deref(), Some("charged"));
    }

    /// Under a GDPR legal hold on `autumn_idempotency_keys`, an expired
    /// response and an expired recovery point stay: the sweep skips, a reused
    /// key is refused instead of taking the row over, and a late `set` does
    /// not overwrite. Without the hold, the same expired key starts over.
    #[tokio::test]
    async fn legal_hold_keeps_expired_rows() {
        let substrate = SqliteSubstrate::with_migrations(&[&crate::migrate::FRAMEWORK_MIGRATIONS])
            .expect("substrate");
        let state = crate::AppState::for_test();
        state.insert_extension(crate::gdpr::GdprRegistry::new().register(
            crate::gdpr::ModelRegistration::retain(TABLE, "litigation hold 2026-CV-1"),
        ));
        let held = DbIdempotencyStore::new(substrate.pool(), Duration::from_secs(60))
            .with_legal_holds(&state);
        let record = |body: &str| IdempotencyRecord {
            status: 201,
            headers: Vec::new(),
            body: body.as_bytes().to_vec(),
            metadata: Vec::new(),
        };
        held.set("rec", "a", record("first"), Vec::new(), Duration::ZERO)
            .await
            .unwrap();
        let mut conn = held.conn().await.unwrap();
        diesel::insert_into(keys::autumn_idempotency_keys)
            .values((
                keys::storage_key.eq("point"),
                keys::recovery_point.eq(Some("charged")),
                keys::locked_until_ms.eq(0),
                keys::expires_at_ms.eq(0),
                keys::ttl_ms.eq(60_000),
            ))
            .execute(&mut conn)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;

        held.writes.store(0, Ordering::Relaxed);
        held.sweep(&mut conn).await;
        drop(conn);
        for key in ["rec", "point"] {
            assert!(
                !held
                    .try_lock(key, "b", Duration::from_secs(60))
                    .await
                    .unwrap(),
                "{key}: a held expired row is not taken over"
            );
        }
        held.set(
            "rec",
            "c",
            record("late"),
            Vec::new(),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        let mut conn = held.conn().await.unwrap();
        let rows: Vec<(String, Option<Vec<u8>>, Option<String>)> = keys::autumn_idempotency_keys
            .select((keys::storage_key, keys::record, keys::recovery_point))
            .order(keys::storage_key)
            .load(&mut conn)
            .await
            .unwrap();
        drop(conn);
        assert_eq!(rows.len(), 2, "both expired rows are kept");
        assert_eq!(rows[0].2.as_deref(), Some("charged"));
        let kept = StoredEntry::decode(rows[1].1.as_deref().expect("record kept")).unwrap();
        assert_eq!(kept.record.body, b"first", "the late set did not overwrite");

        let free = DbIdempotencyStore::new(substrate.pool(), Duration::from_secs(60));
        assert!(
            free.try_lock("rec", "b", Duration::from_secs(60))
                .await
                .unwrap(),
            "without the hold an expired key starts over"
        );
    }

    /// The source key changes hands between the read and the copy: the copy
    /// writes nothing and reports the lost key, so the stale point read
    /// before is never used.
    #[tokio::test]
    async fn copy_refuses_a_source_taken_by_another_request() {
        let substrate = SqliteSubstrate::with_migrations(&[&crate::migrate::FRAMEWORK_MIGRATIONS])
            .expect("substrate");
        let store = DbIdempotencyStore::new(substrate.pool(), Duration::from_secs(60));
        assert!(
            store
                .try_lock("old", "a1", Duration::from_secs(60))
                .await
                .unwrap()
        );
        assert!(
            store
                .try_lock("new", "b", Duration::from_secs(60))
                .await
                .unwrap()
        );
        let mut conn = store.conn().await.unwrap();
        // What a1 read, before another request took the old key and moved
        // its point on.
        let read = || HeldPoint {
            point: Some("charged".to_owned()),
            body_hash: None,
            ttl_ms: 60_000,
        };
        diesel::update(keys::autumn_idempotency_keys.filter(keys::storage_key.eq("old")))
            .set((
                keys::locked_by.eq(Some("a2")),
                keys::recovery_point.eq(Some("shipped")),
            ))
            .execute(&mut conn)
            .await
            .unwrap();

        let copied = store
            .copy_held_point(&mut conn, "old", "a1", read(), "new", "b")
            .await
            .unwrap();
        assert!(!copied, "a1 no longer holds the old key");
        let point: Option<String> = keys::autumn_idempotency_keys
            .filter(keys::storage_key.eq("new"))
            .select(keys::recovery_point)
            .first(&mut conn)
            .await
            .unwrap();
        assert_eq!(point, None, "the stale point was not copied");

        // The holder's own snapshot still copies.
        let held = HeldPoint {
            point: Some("shipped".to_owned()),
            body_hash: None,
            ttl_ms: 60_000,
        };
        assert!(
            store
                .copy_held_point(&mut conn, "old", "a2", held, "new", "b")
                .await
                .unwrap()
        );
    }

    /// The new key's lock expired and another request took it before the
    /// copy. The copy writes nothing and reports `false`, so this request
    /// does not run the handler on a key it no longer holds.
    #[tokio::test]
    async fn copy_refuses_a_target_taken_by_another_request() {
        let substrate = SqliteSubstrate::with_migrations(&[&crate::migrate::FRAMEWORK_MIGRATIONS])
            .expect("substrate");
        let store = DbIdempotencyStore::new(substrate.pool(), Duration::from_secs(60));
        assert!(
            store
                .try_lock("old", "a1", Duration::from_secs(60))
                .await
                .unwrap()
        );
        assert!(
            store
                .try_lock("new", "b", Duration::from_millis(5))
                .await
                .unwrap()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            store
                .try_lock("new", "c", Duration::from_secs(60))
                .await
                .unwrap(),
            "another request takes the new key once b's lock expired"
        );
        let mut conn = store.conn().await.unwrap();
        diesel::update(keys::autumn_idempotency_keys.filter(keys::storage_key.eq("old")))
            .set((
                keys::recovery_point.eq(Some("charged")),
                keys::ttl_ms.eq(60_000),
            ))
            .execute(&mut conn)
            .await
            .unwrap();
        let held = HeldPoint {
            point: Some("charged".to_owned()),
            body_hash: None,
            ttl_ms: 60_000,
        };

        let copied = store
            .copy_held_point(&mut conn, "old", "a1", held, "new", "b")
            .await
            .unwrap();
        assert!(!copied, "b no longer holds the new key");
        let point: Option<String> = keys::autumn_idempotency_keys
            .filter(keys::storage_key.eq("new"))
            .select(keys::recovery_point)
            .first(&mut conn)
            .await
            .unwrap();
        assert_eq!(point, None, "nothing was written to c's key");
    }

    /// The old key has no recovery point, and the new key's lock was lost to
    /// another request. Adoption reports `false`, as it does when there is a
    /// point to copy, so this request does not run the handler.
    #[tokio::test]
    async fn adopt_without_a_point_refuses_a_target_taken_by_another_request() {
        let substrate = SqliteSubstrate::with_migrations(&[&crate::migrate::FRAMEWORK_MIGRATIONS])
            .expect("substrate");
        let store = DbIdempotencyStore::new(substrate.pool(), Duration::from_secs(60));
        assert!(
            store
                .try_lock("old", "a1", Duration::from_secs(60))
                .await
                .unwrap()
        );
        assert!(
            store
                .try_lock("new", "b", Duration::from_millis(5))
                .await
                .unwrap()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            store
                .try_lock("new", "c", Duration::from_secs(60))
                .await
                .unwrap()
        );

        let adopted = store
            .adopt_recovery_point("old", "a1", "new", "b")
            .await
            .unwrap();
        assert!(!adopted, "b no longer holds the new key");
        assert!(
            store
                .adopt_recovery_point("old", "a1", "new", "c")
                .await
                .unwrap(),
            "the holder of both keys goes on"
        );
    }

    /// A TTL too long for the clock's millisecond range still stores and
    /// replays, as it does in the memory store, rather than overflowing the
    /// expiry arithmetic in SQL.
    #[tokio::test]
    async fn a_ttl_past_the_millisecond_range_still_stores() {
        let substrate = SqliteSubstrate::with_migrations(&[&crate::migrate::FRAMEWORK_MIGRATIONS])
            .expect("substrate");
        let store = DbIdempotencyStore::new(substrate.pool(), Duration::MAX);
        huge_ttl_round_trip(&store).await;
    }
}

/// Lock, store and release `key` with a TTL past the millisecond range, then
/// read the record back. Shared by the `SQLite` and Postgres tests.
#[cfg(test)]
async fn huge_ttl_round_trip(store: &DbIdempotencyStore) {
    let record = IdempotencyRecord {
        status: 201,
        headers: Vec::new(),
        body: b"done".to_vec(),
        metadata: Vec::new(),
    };
    assert!(store.try_lock("k", "a", Duration::MAX).await.unwrap());
    store
        .set("k", "a", record, Vec::new(), Duration::MAX)
        .await
        .unwrap();
    store.unlock("k", "a").await.unwrap();
    let entry = store.get("k").await.unwrap();
    assert!(entry.is_some(), "the record replays");
}

/// Postgres-only: the row-lock behaviour `SQLite`, which serialises writers,
/// cannot show.
#[cfg(all(test, not(feature = "sqlite")))]
mod pg_tests {
    use super::*;

    /// Postgres: another request takes the old key in a transaction still
    /// open when the copy runs. The copy waits for that transaction, then
    /// sees the key lost. It does not copy from the old row version that a
    /// snapshot read still sees, which would let both requests resume from
    /// the same point.
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn pg_copy_waits_for_a_source_takeover_in_flight() {
        use diesel_async::SimpleAsyncConnection as _;
        use diesel_async::pooled_connection::AsyncDieselConnectionManager;
        use testcontainers::runners::AsyncRunner as _;
        use testcontainers_modules::postgres::Postgres;

        let container = Postgres::default().start().await.unwrap();
        let port = container.get_host_port_ipv4(5432).await.unwrap();
        let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
        let manager = AsyncDieselConnectionManager::<RuntimeConnection>::new(url);
        let pool = Pool::builder(manager).max_size(4).build().unwrap();
        pool.get()
            .await
            .unwrap()
            .batch_execute(include_str!(
                "../../migrations/20261005200000_create_idempotency_keys/up.sql"
            ))
            .await
            .unwrap();
        let store = Arc::new(DbIdempotencyStore::new(
            pool.clone(),
            Duration::from_secs(60),
        ));
        assert!(
            store
                .try_lock("old", "a1", Duration::from_secs(60))
                .await
                .unwrap()
        );
        assert!(
            store
                .try_lock("new", "b", Duration::from_secs(60))
                .await
                .unwrap()
        );
        let mut taker = pool.get().await.unwrap();
        diesel::update(keys::autumn_idempotency_keys.filter(keys::storage_key.eq("old")))
            .set((
                keys::recovery_point.eq(Some("charged")),
                keys::ttl_ms.eq(60_000),
            ))
            .execute(&mut taker)
            .await
            .unwrap();

        // a2 takes the old key; its transaction has not committed yet.
        taker.batch_execute("BEGIN").await.unwrap();
        diesel::update(keys::autumn_idempotency_keys.filter(keys::storage_key.eq("old")))
            .set(keys::locked_by.eq(Some("a2")))
            .execute(&mut taker)
            .await
            .unwrap();
        let adopt = tokio::spawn({
            let store = Arc::clone(&store);
            async move { store.adopt_recovery_point("old", "a1", "new", "b").await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !adopt.is_finished(),
            "the copy waits for the open takeover of the old key"
        );
        taker.batch_execute("COMMIT").await.unwrap();

        let adopted = adopt.await.unwrap().unwrap();
        assert!(!adopted, "a1 lost the old key to a2");
        let mut conn = pool.get().await.unwrap();
        let point: Option<String> = keys::autumn_idempotency_keys
            .filter(keys::storage_key.eq("new"))
            .select(keys::recovery_point)
            .first(&mut conn)
            .await
            .unwrap();
        assert_eq!(point, None, "the point a2 now holds was not copied");
    }

    /// Postgres: a TTL past the millisecond range does not overflow `BIGINT`
    /// in the expiry arithmetic.
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn pg_a_ttl_past_the_millisecond_range_still_stores() {
        use diesel_async::SimpleAsyncConnection as _;
        use diesel_async::pooled_connection::AsyncDieselConnectionManager;
        use testcontainers::runners::AsyncRunner as _;
        use testcontainers_modules::postgres::Postgres;

        let container = Postgres::default().start().await.unwrap();
        let port = container.get_host_port_ipv4(5432).await.unwrap();
        let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
        let manager = AsyncDieselConnectionManager::<RuntimeConnection>::new(url);
        let pool = Pool::builder(manager).max_size(4).build().unwrap();
        pool.get()
            .await
            .unwrap()
            .batch_execute(include_str!(
                "../../migrations/20261005200000_create_idempotency_keys/up.sql"
            ))
            .await
            .unwrap();
        let store = DbIdempotencyStore::new(pool, Duration::MAX);
        huge_ttl_round_trip(&store).await;
    }
}
