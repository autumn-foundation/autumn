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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

diesel::table! {
    autumn_idempotency_keys (storage_key) {
        storage_key -> Text,
        record -> Nullable<Binary>,
        recovery_point -> Nullable<Text>,
        locked_by -> Nullable<Text>,
        locked_until_ms -> BigInt,
        expires_at_ms -> BigInt,
    }
}

use autumn_idempotency_keys::dsl as keys;

/// Delete expired rows once per this many writes.
const SWEEP_EVERY: u64 = 128;

fn now_ms() -> i64 {
    crate::time::ambient_now().timestamp_millis()
}

fn ms(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
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
pub struct DbIdempotencyStore {
    pool: Pool<RuntimeConnection>,
    default_ttl: Duration,
    writes: AtomicU64,
}

impl DbIdempotencyStore {
    /// Create a store on `pool`. `default_ttl` is the response retention.
    #[must_use]
    pub const fn new(pool: Pool<RuntimeConnection>, default_ttl: Duration) -> Self {
        Self {
            pool,
            default_ttl,
            writes: AtomicU64::new(0),
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
        if let Err(error) =
            diesel::delete(keys::autumn_idempotency_keys.filter(keys::expires_at_ms.le(now_ms())))
                .execute(conn)
                .await
        {
            tracing::warn!(%error, "Idempotency key sweep failed; the next sweep retries");
        }
    }
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
        record: IdempotencyRecord,
        body_hash: Vec<u8>,
        ttl: Duration,
    ) -> IdempotencyFuture<'a, ()> {
        Box::pin(async move {
            let bytes = StoredEntry::encode(record, body_hash)?;
            let expires = after(ttl);
            let mut conn = self.conn().await?;
            diesel::insert_into(keys::autumn_idempotency_keys)
                .values((
                    keys::storage_key.eq(key),
                    keys::record.eq(Some(&bytes)),
                    keys::locked_by.eq(None::<String>),
                    keys::locked_until_ms.eq(0),
                    keys::expires_at_ms.eq(expires),
                ))
                .on_conflict(keys::storage_key)
                .do_update()
                .set((
                    keys::record.eq(Some(&bytes)),
                    keys::locked_by.eq(None::<String>),
                    keys::locked_until_ms.eq(0),
                    keys::expires_at_ms.eq(expires),
                ))
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
            let now = now_ms();
            let lock_ttl = if lock_ttl.is_zero() {
                Duration::from_secs(1)
            } else {
                lock_ttl
            };
            let until = now.saturating_add(ms(lock_ttl));
            let expires = until.max(after(self.default_ttl));
            let mut conn = self.conn().await?;
            self.sweep(&mut conn).await;
            // Check first without a row lock. On Postgres the upsert below
            // waits for any open transaction that changed the row, so a
            // duplicate of a running request must return here, at once.
            let busy = keys::autumn_idempotency_keys
                .filter(keys::storage_key.eq(key))
                .filter(keys::expires_at_ms.gt(now))
                .filter(keys::record.is_not_null().or(keys::locked_until_ms.gt(now)))
                .select(keys::storage_key)
                .first::<String>(&mut conn)
                .await
                .optional()
                .map_err(|e| db_error("read idempotency lock", e))?
                .is_some();
            if busy {
                return Ok(false);
            }
            // An expired key starts over.
            diesel::delete(
                keys::autumn_idempotency_keys
                    .filter(keys::storage_key.eq(key))
                    .filter(keys::expires_at_ms.le(now)),
            )
            .execute(&mut conn)
            .await
            .map_err(|e| db_error("delete expired idempotency key", e))?;
            // Take the lock when the key is new, or has no record and its lock
            // expired. A recovery point stays for the next owner.
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
                    keys::expires_at_ms.eq(expires),
                ));
            // `ON CONFLICT … DO UPDATE … WHERE`: the `WHERE` reads the existing row.
            let acquired = diesel::query_dsl::methods::FilterDsl::filter(
                upsert,
                keys::record.is_null().and(keys::locked_until_ms.le(now)),
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
            diesel::update(held)
                .set((
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
}

impl IdempotencyTx {
    pub(super) fn new(
        storage_key: String,
        owner: String,
        body_hash: Vec<u8>,
        ttl: Duration,
    ) -> Self {
        Self {
            claim: Some(Arc::new(TxClaim {
                storage_key,
                owner,
                body_hash,
                ttl,
                committed: AtomicBool::new(false),
            })),
        }
    }

    /// `true` when the methods write to the database.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.claim.is_some()
    }

    /// `true` after [`Self::commit`] wrote the record.
    pub(super) fn committed(&self) -> bool {
        self.claim
            .as_ref()
            .is_some_and(|claim| AtomicBool::load(&claim.committed, Ordering::SeqCst))
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
    /// - `500` when the body is larger than 10 MiB or cannot be read, or the
    ///   write fails.
    pub fn commit<'c>(
        &self,
        conn: &'c mut RuntimeConnection,
        response: impl IntoResponse,
    ) -> impl Future<Output = AutumnResult<Response<Body>>> + Send + 'c {
        let response = response.into_response();
        let claim = self.claim.clone();
        async move {
            let Some(claim) = claim else {
                return Ok(response);
            };
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
            let written = diesel::update(owned_key(&claim))
                .set((
                    keys::record.eq(Some(encoded)),
                    keys::expires_at_ms.eq(after(claim.ttl)),
                ))
                .execute(conn)
                .await?;
            if written != 1 {
                return Err(claim_error(conn, &claim).await);
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
    /// `409` when this request no longer holds the key. `500` when the write
    /// fails.
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
            let written = diesel::update(owned_key(&claim))
                .set(keys::recovery_point.eq(Some(point)))
                .execute(conn)
                .await?;
            if written == 1 {
                Ok(())
            } else {
                Err(claim_error(conn, &claim).await)
            }
        }
    }

    /// The last recovery point of this key, or `None` if no step committed.
    /// Always `None` when inactive.
    ///
    /// # Errors
    ///
    /// `500` when the read fails.
    pub fn recovery_point<'c>(
        &self,
        conn: &'c mut RuntimeConnection,
    ) -> impl Future<Output = AutumnResult<Option<String>>> + Send + 'c {
        let claim = self.claim.clone();
        async move {
            let Some(claim) = claim else {
                return Ok(None);
            };
            let point: Option<Option<String>> = keys::autumn_idempotency_keys
                .filter(keys::storage_key.eq(&claim.storage_key))
                .select(keys::recovery_point)
                .first(conn)
                .await
                .optional()?;
            Ok(point.flatten())
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
        Ok(parts.extensions.get::<Self>().cloned().unwrap_or_default())
    }
}
