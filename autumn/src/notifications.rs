//! First-class in-app notification store with a read/unread feed (issue
//! #1148).
//!
//! Nearly every account-based app grows a "notification bell": a persistent,
//! per-recipient feed with read/unread state and an unread count. This module
//! ships that composition on top of the primitives Autumn already provides —
//! the database layer for persistence, [`Page`]/[`ListQuery`] for the feed,
//! and (optionally) `channels` for realtime push — so an app adds a feed with
//! one `notify(...)` call instead of a hand-rolled table and four queries.
//!
//! # Quick start
//!
//! Declare the [`Notifications`] extractor in any handler:
//!
//! ```rust,ignore
//! use autumn_web::notifications::Notifications;
//! use autumn_web::prelude::*;
//!
//! #[post("/comments")]
//! async fn create_comment(notifications: Notifications) -> AutumnResult<&'static str> {
//!     // ... create the comment ...
//!     notifications
//!         .notify(recipient_id, "comment.created", serde_json::json!({"post": 42}))
//!         .await?;
//!     Ok("ok")
//! }
//! ```
//!
//! Reading the feed reuses the shipped pagination types:
//!
//! ```rust,ignore
//! #[get("/notifications")]
//! async fn feed(
//!     Auth(user): Auth<CurrentUser>,
//!     query: ListQuery,
//!     page: PageRequest,
//!     notifications: Notifications,
//! ) -> AutumnResult<Json<Page<Notification>>> {
//!     Ok(Json(notifications.list(user.id, &query, &page).await?))
//! }
//! ```
//!
//! # Storage backends
//!
//! [`Notifications`] delegates to a pluggable [`NotificationStore`], resolved
//! per app in this order:
//!
//! 1. A store registered via `AppBuilder::with_notification_store(...)`.
//! 2. [`DbNotificationStore`] when a database pool is configured (the
//!    `notifications` table is scaffolded by `autumn generate notifications`).
//! 3. [`MemoryNotificationStore`] otherwise — a process-local fallback for
//!    tests and DB-less development.
//!
//! This mirrors how `Session` resolves its `SessionStore`.
//!
//! # Realtime push over `channels` (optional, best-effort)
//!
//! With the `ws` feature enabled, [`Notifications::notify_with_push`] persists
//! the notification and then publishes its JSON on the conventional
//! per-recipient topic [`Notifications::topic`] (`notifications:{recipient}`).
//! The broadcast is **best-effort**: a channel failure (including the common
//! "no subscribers" case) never fails `notify`. Equivalent manual wiring:
//!
//! ```rust,ignore
//! let n = notifications.notify(user.id, "like", payload).await?;
//! // Best-effort: ignore the publish result.
//! let _ = state
//!     .broadcast()
//!     .publish(&Notifications::topic(user.id), serde_json::to_string(&n)?);
//! ```
//!
//! # List filters and ordering
//!
//! [`Notifications::list`] understands two equality filters from
//! [`ListQuery`]: `unread` (`filter[unread]=true` keeps only rows whose
//! `read_at` is unset) and `kind` (`filter[kind]=comment.created`). Sorting
//! accepts `id` or `created_at`; without an explicit `sort=` the feed returns
//! newest-first. Unknown filters and sort keys are ignored, matching the
//! repository `list()` contract.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{AutumnError, AutumnResult};
use crate::pagination::{ListQuery, Page, PageRequest, SortDir};
use crate::state::AppState;

/// The `notifications` table name shared by [`DbNotificationStore`] and the
/// `autumn generate notifications` scaffolding.
pub const NOTIFICATIONS_TABLE: &str = "notifications";

// ── Record ──────────────────────────────────────────────────────────────────

/// A persisted per-recipient notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    /// Primary key.
    pub id: i64,
    /// The user this notification belongs to.
    pub recipient_id: i64,
    /// Application-defined discriminator, e.g. `"comment.created"`.
    pub kind: String,
    /// Application-defined JSON payload.
    pub payload: Value,
    /// When the notification was marked read; `None` while unread.
    pub read_at: Option<DateTime<Utc>>,
    /// Creation time.
    pub created_at: DateTime<Utc>,
}

impl Notification {
    /// `true` once the notification has been marked read.
    #[must_use]
    pub const fn is_read(&self) -> bool {
        self.read_at.is_some()
    }
}

// ── Store error ─────────────────────────────────────────────────────────────

/// Error returned by [`NotificationStore`] implementations.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[error("notification store error: {message}")]
pub struct NotificationStoreError {
    message: String,
}

impl NotificationStoreError {
    /// Create a new error with the given message.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

// ── Store trait ─────────────────────────────────────────────────────────────

/// Pluggable notification storage backend.
///
/// Implement this to store notifications somewhere other than the built-in
/// backends. All read/unread semantics live in the store so every backend
/// behaves identically:
///
/// - `unread_count` and the `unread` list filter count only rows whose
///   `read_at` is unset.
/// - `mark_read`/`mark_all_read` are idempotent: re-marking a read
///   notification is a no-op (it must not error and must not overwrite the
///   original `read_at`).
pub trait NotificationStore: Send + Sync + 'static {
    /// Persist a new, unread notification and return the stored record.
    fn notify(
        &self,
        recipient_id: i64,
        kind: String,
        payload: Value,
    ) -> impl Future<Output = Result<Notification, NotificationStoreError>> + Send;

    /// One page of the recipient's feed. See the module docs for the
    /// supported filters (`unread`, `kind`) and sort keys (`id`,
    /// `created_at`; default newest-first).
    fn list(
        &self,
        recipient_id: i64,
        query: &ListQuery,
        page: &PageRequest,
    ) -> impl Future<Output = Result<Page<Notification>, NotificationStoreError>> + Send;

    /// Number of unread notifications for the recipient.
    fn unread_count(
        &self,
        recipient_id: i64,
    ) -> impl Future<Output = Result<u64, NotificationStoreError>> + Send;

    /// Mark one notification read, optionally scoped to a recipient. Returns
    /// the number of rows transitioned (0 when already read, missing, or —
    /// with a recipient scope — owned by someone else).
    fn mark_read(
        &self,
        id: i64,
        recipient_id: Option<i64>,
    ) -> impl Future<Output = Result<u64, NotificationStoreError>> + Send;

    /// Mark all of the recipient's unread notifications read. Returns the
    /// number of rows transitioned.
    fn mark_all_read(
        &self,
        recipient_id: i64,
    ) -> impl Future<Output = Result<u64, NotificationStoreError>> + Send;
}

// ── Erasure bridge (same shape as session::BoxedSessionStore) ───────────────
//
// `NotificationStore` uses RPIT futures and is not dyn-compatible, so
// [`Notifications`] holds a pub(crate) shadow trait with a blanket impl.
// Users only ever see `NotificationStore`.

type BoxedFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, NotificationStoreError>> + Send + 'a>>;

pub(crate) trait BoxedNotificationStore: Send + Sync + 'static {
    fn boxed_notify(
        &self,
        recipient_id: i64,
        kind: String,
        payload: Value,
    ) -> BoxedFuture<'_, Notification>;
    fn boxed_list(
        &self,
        recipient_id: i64,
        query: ListQuery,
        page: PageRequest,
    ) -> BoxedFuture<'_, Page<Notification>>;
    fn boxed_unread_count(&self, recipient_id: i64) -> BoxedFuture<'_, u64>;
    fn boxed_mark_read(&self, id: i64, recipient_id: Option<i64>) -> BoxedFuture<'_, u64>;
    fn boxed_mark_all_read(&self, recipient_id: i64) -> BoxedFuture<'_, u64>;
}

impl<S: NotificationStore> BoxedNotificationStore for S {
    fn boxed_notify(
        &self,
        recipient_id: i64,
        kind: String,
        payload: Value,
    ) -> BoxedFuture<'_, Notification> {
        Box::pin(NotificationStore::notify(self, recipient_id, kind, payload))
    }

    fn boxed_list(
        &self,
        recipient_id: i64,
        query: ListQuery,
        page: PageRequest,
    ) -> BoxedFuture<'_, Page<Notification>> {
        Box::pin(async move { NotificationStore::list(self, recipient_id, &query, &page).await })
    }

    fn boxed_unread_count(&self, recipient_id: i64) -> BoxedFuture<'_, u64> {
        Box::pin(NotificationStore::unread_count(self, recipient_id))
    }

    fn boxed_mark_read(&self, id: i64, recipient_id: Option<i64>) -> BoxedFuture<'_, u64> {
        Box::pin(NotificationStore::mark_read(self, id, recipient_id))
    }

    fn boxed_mark_all_read(&self, recipient_id: i64) -> BoxedFuture<'_, u64> {
        Box::pin(NotificationStore::mark_all_read(self, recipient_id))
    }
}

// ── Service / extractor ─────────────────────────────────────────────────────

/// The in-app notifications service.
///
/// Declare it as a handler parameter (it implements `FromRequestParts`, like
/// [`Session`](crate::session::Session)) or construct one directly with
/// [`Notifications::new`] in tests and background jobs. See the module docs
/// for store resolution and examples.
#[derive(Clone)]
pub struct Notifications {
    store: Arc<dyn BoxedNotificationStore>,
    #[cfg(feature = "ws")]
    channels: Option<crate::channels::Channels>,
}

impl Notifications {
    /// Build a service over an explicit store.
    #[must_use]
    pub fn new(store: impl NotificationStore) -> Self {
        Self {
            store: Arc::new(store),
            #[cfg(feature = "ws")]
            channels: None,
        }
    }

    /// The conventional per-recipient channel topic
    /// (`notifications:{recipient_id}`) used by
    /// [`notify_with_push`](Self::notify_with_push) and available to
    /// subscribers (e.g. a WebSocket feed handler).
    #[must_use]
    pub fn topic(recipient_id: i64) -> String {
        format!("notifications:{recipient_id}")
    }

    /// Persist a new, unread notification for `recipient_id` and return it.
    ///
    /// The write is immediately visible to [`list`](Self::list) and
    /// [`unread_count`](Self::unread_count).
    ///
    /// # Errors
    ///
    /// Returns a 500 [`AutumnError`] when the underlying store fails.
    pub async fn notify(
        &self,
        recipient_id: i64,
        kind: impl Into<String>,
        payload: Value,
    ) -> AutumnResult<Notification> {
        self.store
            .boxed_notify(recipient_id, kind.into(), payload)
            .await
            .map_err(AutumnError::internal_server_error)
    }

    /// [`notify`](Self::notify), then publish the stored notification as JSON
    /// on [`Notifications::topic`] over the app's `channels` transport.
    ///
    /// The broadcast is **best-effort**: any publish failure (most commonly
    /// "no subscribers on the topic") is ignored and never fails the notify.
    /// Outside of a request context (a service built with
    /// [`Notifications::new`]) there is no channel registry and the push is
    /// skipped entirely.
    ///
    /// # Errors
    ///
    /// Returns a 500 [`AutumnError`] when the underlying store fails —
    /// never because of the channel publish.
    #[cfg(feature = "ws")]
    pub async fn notify_with_push(
        &self,
        recipient_id: i64,
        kind: impl Into<String>,
        payload: Value,
    ) -> AutumnResult<Notification> {
        let notification = self.notify(recipient_id, kind, payload).await?;
        if let Some(channels) = &self.channels
            && let Ok(json) = serde_json::to_string(&notification)
        {
            // Best-effort: a dead or subscriber-less channel never fails
            // notify.
            let _ = channels
                .broadcast()
                .publish(&Self::topic(recipient_id), json);
        }
        Ok(notification)
    }

    /// One page of the recipient's feed. See the module docs for the
    /// supported [`ListQuery`] filters and sort keys.
    ///
    /// # Errors
    ///
    /// Returns a 500 [`AutumnError`] when the underlying store fails.
    pub async fn list(
        &self,
        recipient_id: i64,
        query: &ListQuery,
        page: &PageRequest,
    ) -> AutumnResult<Page<Notification>> {
        self.store
            .boxed_list(recipient_id, query.clone(), *page)
            .await
            .map_err(AutumnError::internal_server_error)
    }

    /// Number of unread notifications for the recipient.
    ///
    /// # Errors
    ///
    /// Returns a 500 [`AutumnError`] when the underlying store fails.
    pub async fn unread_count(&self, recipient_id: i64) -> AutumnResult<u64> {
        self.store
            .boxed_unread_count(recipient_id)
            .await
            .map_err(AutumnError::internal_server_error)
    }

    /// Mark one notification read. Idempotent: re-marking a read notification
    /// (or a missing id) is a no-op, and the original `read_at` is preserved.
    ///
    /// This variant is unscoped — any notification id can be marked. In a
    /// handler acting on behalf of a signed-in user prefer
    /// [`mark_read_for`](Self::mark_read_for), which refuses to touch other
    /// recipients' notifications.
    ///
    /// # Errors
    ///
    /// Returns a 500 [`AutumnError`] when the underlying store fails.
    pub async fn mark_read(&self, id: i64) -> AutumnResult<()> {
        self.store
            .boxed_mark_read(id, None)
            .await
            .map(|_| ())
            .map_err(AutumnError::internal_server_error)
    }

    /// Mark one of `recipient_id`'s own notifications read. A notification
    /// owned by a different recipient is left untouched (a no-op, not an
    /// error), making this the safe choice for user-facing handlers.
    ///
    /// # Errors
    ///
    /// Returns a 500 [`AutumnError`] when the underlying store fails.
    pub async fn mark_read_for(&self, recipient_id: i64, id: i64) -> AutumnResult<()> {
        self.store
            .boxed_mark_read(id, Some(recipient_id))
            .await
            .map(|_| ())
            .map_err(AutumnError::internal_server_error)
    }

    /// Mark all of the recipient's unread notifications read and return how
    /// many transitioned. Idempotent: a second call returns 0.
    ///
    /// # Errors
    ///
    /// Returns a 500 [`AutumnError`] when the underlying store fails.
    pub async fn mark_all_read(&self, recipient_id: i64) -> AutumnResult<u64> {
        self.store
            .boxed_mark_all_read(recipient_id)
            .await
            .map_err(AutumnError::internal_server_error)
    }

    /// Default service for an app that never registered a store: the
    /// database-backed store when a pool is configured, the in-memory store
    /// otherwise.
    fn default_for(state: &AppState) -> Self {
        #[cfg(feature = "db")]
        if let Some(pool) = crate::db::DbState::pool(state) {
            return Self::new(DbNotificationStore::new(pool.clone()));
        }
        Self::new(MemoryNotificationStore::new())
    }
}

impl std::fmt::Debug for Notifications {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Notifications").finish_non_exhaustive()
    }
}

impl axum::extract::FromRequestParts<AppState> for Notifications {
    // Resolution always succeeds (a store fallback always exists); failures
    // surface from the service methods, not extraction — same contract as
    // `Session`.
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        _parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // `extension_or_insert_with` keeps the resolved service (and thus the
        // memory store's contents) stable across requests, and lets
        // `AppBuilder::with_notification_store` pre-empt the default.
        let service = state.extension_or_insert_with::<Self>(|| Self::default_for(state));
        #[cfg_attr(not(feature = "ws"), allow(unused_mut))]
        let mut service = (*service).clone();
        #[cfg(feature = "ws")]
        {
            service.channels = Some(state.channels().clone());
        }
        Ok(service)
    }
}

// ── In-memory store ─────────────────────────────────────────────────────────

/// Process-local [`NotificationStore`] used by default when no database is
/// configured.
///
/// Suitable for tests and DB-less development; contents are lost on restart
/// and the store grows without bound (no eviction), so it is not a
/// production store.
#[derive(Debug, Default)]
pub struct MemoryNotificationStore {
    inner: std::sync::Mutex<MemoryInner>,
}

#[derive(Debug, Default)]
struct MemoryInner {
    next_id: i64,
    rows: Vec<Notification>,
}

impl MemoryNotificationStore {
    /// Create an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, MemoryInner>, NotificationStoreError> {
        self.inner
            .lock()
            .map_err(|_| NotificationStoreError::new("memory store mutex poisoned"))
    }
}

impl NotificationStore for MemoryNotificationStore {
    async fn notify(
        &self,
        recipient_id: i64,
        kind: String,
        payload: Value,
    ) -> Result<Notification, NotificationStoreError> {
        let mut inner = self.lock()?;
        inner.next_id += 1;
        let notification = Notification {
            id: inner.next_id,
            recipient_id,
            kind,
            payload,
            read_at: None,
            created_at: Utc::now(),
        };
        inner.rows.push(notification.clone());
        drop(inner);
        Ok(notification)
    }

    async fn list(
        &self,
        recipient_id: i64,
        query: &ListQuery,
        page: &PageRequest,
    ) -> Result<Page<Notification>, NotificationStoreError> {
        let filter = ListFilter::from_query(query);
        let inner = self.lock()?;
        let mut rows: Vec<Notification> = inner
            .rows
            .iter()
            .filter(|n| n.recipient_id == recipient_id)
            .filter(|n| !filter.unread_only || n.read_at.is_none())
            .filter(|n| filter.kind.as_deref().is_none_or(|k| n.kind == k))
            .cloned()
            .collect();
        drop(inner);

        // `id` is assigned monotonically, so it doubles as insertion order and
        // as the tiebreaker for equal `created_at` values.
        match filter.sort {
            FeedSort::CreatedAt => rows.sort_by_key(|n| (n.created_at, n.id)),
            FeedSort::Id => rows.sort_by_key(|n| n.id),
        }
        if filter.dir == SortDir::Desc {
            rows.reverse();
        }

        let total = i64::try_from(rows.len()).unwrap_or(i64::MAX);
        let start = usize::try_from(page.offset()).unwrap_or(usize::MAX);
        let items: Vec<Notification> = rows
            .into_iter()
            .skip(start)
            .take(usize::try_from(page.limit()).unwrap_or(0))
            .collect();
        Ok(Page::new(items, total, page))
    }

    async fn unread_count(&self, recipient_id: i64) -> Result<u64, NotificationStoreError> {
        let inner = self.lock()?;
        let count = inner
            .rows
            .iter()
            .filter(|n| n.recipient_id == recipient_id && n.read_at.is_none())
            .count();
        drop(inner);
        Ok(count as u64)
    }

    async fn mark_read(
        &self,
        id: i64,
        recipient_id: Option<i64>,
    ) -> Result<u64, NotificationStoreError> {
        let now = Utc::now();
        let mut inner = self.lock()?;
        let marked = inner
            .rows
            .iter_mut()
            .filter(|n| n.id == id)
            .filter(|n| recipient_id.is_none_or(|rid| n.recipient_id == rid))
            .filter(|n| n.read_at.is_none())
            .map(|n| n.read_at = Some(now))
            .count();
        drop(inner);
        Ok(marked as u64)
    }

    async fn mark_all_read(&self, recipient_id: i64) -> Result<u64, NotificationStoreError> {
        let now = Utc::now();
        let mut inner = self.lock()?;
        let marked = inner
            .rows
            .iter_mut()
            .filter(|n| n.recipient_id == recipient_id && n.read_at.is_none())
            .map(|n| n.read_at = Some(now))
            .count();
        drop(inner);
        Ok(marked as u64)
    }
}

// ── Shared list-parameter interpretation ────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FeedSort {
    Id,
    CreatedAt,
}

/// The [`ListQuery`] subset both stores understand, so the memory and
/// database backends interpret feed parameters identically.
struct ListFilter {
    unread_only: bool,
    kind: Option<String>,
    sort: FeedSort,
    dir: SortDir,
}

impl ListFilter {
    fn from_query(query: &ListQuery) -> Self {
        let mut unread_only = false;
        let mut kind = None;
        for (column, value) in query.filters() {
            match column {
                "unread" => unread_only = value == "true" || value == "1",
                "kind" => kind = Some(value.to_owned()),
                // Unknown filters are ignored, matching the repository
                // `list()` allowlist contract.
                _ => {}
            }
        }
        // Without an explicit sort the feed reads newest-first; an explicit
        // sort key uses the requested direction (`ListQuery` defaults to
        // ascending).
        let (sort, dir) = match query.sort() {
            Some("created_at") => (FeedSort::CreatedAt, query.direction()),
            Some("id") => (FeedSort::Id, query.direction()),
            // No sort requested, or a key outside the allowlist: newest-first.
            None | Some(_) => (FeedSort::Id, SortDir::Desc),
        };
        Self {
            unread_only,
            kind,
            sort,
            dir,
        }
    }
}

// ── Database-backed store ───────────────────────────────────────────────────

#[cfg(feature = "db")]
pub use self::db_store::DbNotificationStore;

#[cfg(feature = "db")]
mod db_store {
    use super::{
        FeedSort, ListFilter, Notification, NotificationStore, NotificationStoreError, Value,
    };
    use crate::db::RuntimeConnection;
    use crate::pagination::{ListQuery, Page, PageRequest, SortDir};
    use chrono::{DateTime, Utc};
    use diesel::prelude::*;
    use diesel_async::RunQueryDsl;
    use diesel_async::pooled_connection::deadpool::Pool;

    // The `notifications` table as scaffolded by `autumn generate
    // notifications`. Timestamps are TIMESTAMPTZ on Postgres and RFC 3339
    // TEXT on SQLite (diesel's `TimestamptzSqlite`), matching the DDL the
    // generator emits per backend.
    #[cfg(not(feature = "sqlite"))]
    mod schema {
        diesel::table! {
            notifications (id) {
                id -> BigInt,
                recipient_id -> BigInt,
                kind -> Text,
                payload -> Text,
                read_at -> Nullable<Timestamptz>,
                created_at -> Timestamptz,
            }
        }
    }
    #[cfg(feature = "sqlite")]
    mod schema {
        diesel::table! {
            notifications (id) {
                id -> BigInt,
                recipient_id -> BigInt,
                kind -> Text,
                payload -> Text,
                read_at -> Nullable<TimestamptzSqlite>,
                created_at -> TimestamptzSqlite,
            }
        }
    }

    use schema::notifications;

    #[derive(Queryable, Selectable)]
    #[diesel(table_name = notifications)]
    struct NotificationRow {
        id: i64,
        recipient_id: i64,
        kind: String,
        payload: String,
        read_at: Option<DateTime<Utc>>,
        created_at: DateTime<Utc>,
    }

    impl From<NotificationRow> for Notification {
        fn from(row: NotificationRow) -> Self {
            // A payload that no longer parses as JSON (e.g. edited out of
            // band) degrades to a JSON string of the raw text rather than
            // failing the whole feed.
            let payload = match serde_json::from_str(&row.payload) {
                Ok(parsed) => parsed,
                Err(_) => Value::String(row.payload),
            };
            Self {
                id: row.id,
                recipient_id: row.recipient_id,
                kind: row.kind,
                payload,
                read_at: row.read_at,
                created_at: row.created_at,
            }
        }
    }

    #[derive(Insertable)]
    #[diesel(table_name = notifications)]
    struct NewNotificationRow {
        recipient_id: i64,
        kind: String,
        payload: String,
        created_at: DateTime<Utc>,
    }

    /// [`NotificationStore`] backed by the app's database pool.
    ///
    /// Expects the `notifications` table scaffolded by
    /// `autumn generate notifications`. This is the default store whenever
    /// the app has a database configured.
    #[derive(Clone)]
    pub struct DbNotificationStore {
        pool: Pool<RuntimeConnection>,
    }

    impl DbNotificationStore {
        /// Build a store over the given pool.
        #[must_use]
        pub const fn new(pool: Pool<RuntimeConnection>) -> Self {
            Self { pool }
        }

        async fn conn(
            &self,
        ) -> Result<
            diesel_async::pooled_connection::deadpool::Object<RuntimeConnection>,
            NotificationStoreError,
        > {
            self.pool
                .get()
                .await
                .map_err(|e| NotificationStoreError::new(format!("checkout failed: {e}")))
        }
    }

    fn store_err(e: &diesel::result::Error) -> NotificationStoreError {
        let message = e.to_string();
        // A missing table means the app has a database but never scaffolded
        // the notifications feed — turn the bare SQL error into an
        // actionable one ("relation … does not exist" on Postgres, "no such
        // table" on SQLite).
        if message.contains("does not exist") || message.contains("no such table") {
            return NotificationStoreError::new(format!(
                "query failed: {e}. The `notifications` table is missing — scaffold it \
                 with `autumn generate notifications`, then apply it with `autumn migrate`"
            ));
        }
        NotificationStoreError::new(format!("query failed: {e}"))
    }

    impl NotificationStore for DbNotificationStore {
        async fn notify(
            &self,
            recipient_id: i64,
            kind: String,
            payload: Value,
        ) -> Result<Notification, NotificationStoreError> {
            let payload = serde_json::to_string(&payload)
                .map_err(|e| NotificationStoreError::new(format!("payload serialization: {e}")))?;
            let mut conn = self.conn().await?;
            let row: NotificationRow = diesel::insert_into(notifications::table)
                .values(NewNotificationRow {
                    recipient_id,
                    kind,
                    payload,
                    created_at: Utc::now(),
                })
                .returning(NotificationRow::as_returning())
                .get_result(&mut conn)
                .await
                .map_err(|e| store_err(&e))?;
            Ok(row.into())
        }

        async fn list(
            &self,
            recipient_id: i64,
            query: &ListQuery,
            page: &PageRequest,
        ) -> Result<Page<Notification>, NotificationStoreError> {
            use notifications::dsl;

            let filter = ListFilter::from_query(query);
            let mut conn = self.conn().await?;

            let mut count_query = dsl::notifications
                .filter(dsl::recipient_id.eq(recipient_id))
                .into_boxed();
            let mut select_query = dsl::notifications
                .filter(dsl::recipient_id.eq(recipient_id))
                .into_boxed();
            if filter.unread_only {
                count_query = count_query.filter(dsl::read_at.is_null());
                select_query = select_query.filter(dsl::read_at.is_null());
            }
            if let Some(kind) = &filter.kind {
                count_query = count_query.filter(dsl::kind.eq(kind.clone()));
                select_query = select_query.filter(dsl::kind.eq(kind.clone()));
            }

            let total: i64 = count_query
                .count()
                .get_result(&mut conn)
                .await
                .map_err(|e| store_err(&e))?;

            select_query = match (filter.sort, filter.dir) {
                (FeedSort::Id, SortDir::Asc) => select_query.order(dsl::id.asc()),
                (FeedSort::Id, SortDir::Desc) => select_query.order(dsl::id.desc()),
                (FeedSort::CreatedAt, SortDir::Asc) => {
                    select_query.order((dsl::created_at.asc(), dsl::id.asc()))
                }
                (FeedSort::CreatedAt, SortDir::Desc) => {
                    select_query.order((dsl::created_at.desc(), dsl::id.desc()))
                }
            };

            let rows: Vec<NotificationRow> = select_query
                .select(NotificationRow::as_select())
                .limit(page.limit())
                .offset(page.offset())
                .load(&mut conn)
                .await
                .map_err(|e| store_err(&e))?;

            Ok(Page::new(
                rows.into_iter().map(Notification::from).collect(),
                total,
                page,
            ))
        }

        async fn unread_count(&self, recipient_id: i64) -> Result<u64, NotificationStoreError> {
            use notifications::dsl;
            let mut conn = self.conn().await?;
            let count: i64 = dsl::notifications
                .filter(dsl::recipient_id.eq(recipient_id))
                .filter(dsl::read_at.is_null())
                .count()
                .get_result(&mut conn)
                .await
                .map_err(|e| store_err(&e))?;
            Ok(u64::try_from(count).unwrap_or(0))
        }

        async fn mark_read(
            &self,
            id: i64,
            recipient_id: Option<i64>,
        ) -> Result<u64, NotificationStoreError> {
            use notifications::dsl;
            let mut conn = self.conn().await?;
            let now = Utc::now();
            // `read_at IS NULL` in the predicate makes the update idempotent:
            // an already-read row matches zero rows and keeps its original
            // timestamp.
            let affected = match recipient_id {
                Some(rid) => {
                    diesel::update(
                        dsl::notifications
                            .filter(dsl::id.eq(id))
                            .filter(dsl::recipient_id.eq(rid))
                            .filter(dsl::read_at.is_null()),
                    )
                    .set(dsl::read_at.eq(Some(now)))
                    .execute(&mut conn)
                    .await
                }
                None => {
                    diesel::update(
                        dsl::notifications
                            .filter(dsl::id.eq(id))
                            .filter(dsl::read_at.is_null()),
                    )
                    .set(dsl::read_at.eq(Some(now)))
                    .execute(&mut conn)
                    .await
                }
            }
            .map_err(|e| store_err(&e))?;
            Ok(affected as u64)
        }

        async fn mark_all_read(&self, recipient_id: i64) -> Result<u64, NotificationStoreError> {
            use notifications::dsl;
            let mut conn = self.conn().await?;
            let affected = diesel::update(
                dsl::notifications
                    .filter(dsl::recipient_id.eq(recipient_id))
                    .filter(dsl::read_at.is_null()),
            )
            .set(dsl::read_at.eq(Some(Utc::now())))
            .execute(&mut conn)
            .await
            .map_err(|e| store_err(&e))?;
            Ok(affected as u64)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn list_filter_defaults_to_newest_first() {
        let filter = ListFilter::from_query(&ListQuery::default());
        assert!(!filter.unread_only);
        assert!(filter.kind.is_none());
        assert_eq!(filter.sort, FeedSort::Id);
        assert_eq!(filter.dir, SortDir::Desc);
    }

    #[test]
    fn list_filter_parses_unread_and_kind() {
        let query = ListQuery::new(None, SortDir::Asc, &[("unread", "1"), ("kind", "like")]);
        let filter = ListFilter::from_query(&query);
        assert!(filter.unread_only);
        assert_eq!(filter.kind.as_deref(), Some("like"));
    }

    #[test]
    fn list_filter_ignores_unknown_sort_and_filters() {
        let query = ListQuery::new(Some("payload"), SortDir::Asc, &[("recipient_id", "9")]);
        let filter = ListFilter::from_query(&query);
        assert!(!filter.unread_only);
        assert!(filter.kind.is_none());
        assert_eq!(filter.sort, FeedSort::Id);
        assert_eq!(
            filter.dir,
            SortDir::Desc,
            "unknown sort falls back to newest-first"
        );
    }

    #[test]
    fn explicit_sort_uses_requested_direction() {
        let query = ListQuery::new(Some("created_at"), SortDir::Asc, &[]);
        let filter = ListFilter::from_query(&query);
        assert_eq!(filter.sort, FeedSort::CreatedAt);
        assert_eq!(filter.dir, SortDir::Asc);
    }

    #[tokio::test]
    async fn memory_store_round_trips_payload() {
        let notifications = Notifications::new(MemoryNotificationStore::new());
        let n = notifications
            .notify(1, "k", json!({"deep": {"value": [1, 2, 3]}}))
            .await
            .expect("notify");
        assert_eq!(n.payload, json!({"deep": {"value": [1, 2, 3]}}));
    }
}
