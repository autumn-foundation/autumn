//! Server side of the sync protocol: backends + mountable router.

use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};

use super::SyncError;
use super::protocol::{PullQuery, PullResponse, PushRequest, PushResponse, Version};
use super::resolver::ConflictResolver;

/// Storage backend for the server sync endpoints.
///
/// Implementations must apply each push batch atomically (all-or-nothing)
/// and dedup on `(device_id, change_id)` so retries are idempotent.
/// Methods are synchronous; the router runs them on the blocking pool.
pub trait SyncBackend: Send + Sync + 'static {
    /// Apply one push batch atomically, returning one outcome per change
    /// (request order). Conflicts are settled via `resolver` and assigned a
    /// new version.
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Backend`] on storage failure (the whole batch
    /// rolls back).
    fn apply_push(
        &self,
        request: &PushRequest,
        resolver: &dyn ConflictResolver,
    ) -> Result<PushResponse, SyncError>;

    /// Return rows with version greater than `cursor` (ascending, at most
    /// `limit`), or `FullResyncRequired` when `cursor` predates the
    /// tombstone GC horizon.
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Backend`] on storage failure.
    fn pull_since(&self, cursor: Version, limit: i64) -> Result<PullResponse, SyncError>;

    /// Physically drop tombstone rows with version at or below `up_to` and
    /// advance the tombstone horizon. Returns the number of rows removed.
    /// Clients whose cursor then trails the horizon get a full resync.
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Backend`] on storage failure.
    fn gc_tombstones(&self, up_to: Version) -> Result<u64, SyncError>;

    /// The current tombstone GC horizon (`0` if GC never ran).
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Backend`] on storage failure.
    fn tombstone_horizon(&self) -> Result<Version, SyncError>;

    /// The most recently assigned version (`0` if nothing was ever pushed).
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Backend`] on storage failure.
    fn latest_version(&self) -> Result<Version, SyncError>;
}

/// In-memory [`SyncBackend`] for tests, demos, and single-process setups.
#[derive(Default)]
pub struct MemorySyncBackend {
    #[allow(dead_code)]
    state: Mutex<()>,
}

impl MemorySyncBackend {
    /// An empty in-memory backend.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl SyncBackend for MemorySyncBackend {
    fn apply_push(
        &self,
        request: &PushRequest,
        resolver: &dyn ConflictResolver,
    ) -> Result<PushResponse, SyncError> {
        let _ = (request, resolver);
        Err(SyncError::Backend("unimplemented".into()))
    }

    fn pull_since(&self, cursor: Version, limit: i64) -> Result<PullResponse, SyncError> {
        let _ = (cursor, limit);
        Err(SyncError::Backend("unimplemented".into()))
    }

    fn gc_tombstones(&self, up_to: Version) -> Result<u64, SyncError> {
        let _ = up_to;
        Err(SyncError::Backend("unimplemented".into()))
    }

    fn tombstone_horizon(&self) -> Result<Version, SyncError> {
        Err(SyncError::Backend("unimplemented".into()))
    }

    fn latest_version(&self) -> Result<Version, SyncError> {
        Err(SyncError::Backend("unimplemented".into()))
    }
}

/// Postgres-backed [`SyncBackend`] persisting into the `autumn_sync_*`
/// shadow tables.
#[derive(Debug, Clone)]
pub struct PgSyncBackend {
    #[allow(dead_code)]
    database_url: String,
}

impl PgSyncBackend {
    /// A backend connecting to `database_url`. Call [`Self::ensure_schema`]
    /// once at startup before serving.
    #[must_use]
    pub fn new(database_url: impl Into<String>) -> Self {
        Self {
            database_url: database_url.into(),
        }
    }

    /// Create the `autumn_sync_rows` / `autumn_sync_applied` /
    /// `autumn_sync_meta` shadow tables and the version sequence if they do
    /// not exist. Idempotent; deliberately not part of the framework
    /// migrations so non-sync apps see zero schema churn.
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Backend`] if the DDL cannot be applied.
    pub fn ensure_schema(&self) -> Result<(), SyncError> {
        Err(SyncError::Backend("unimplemented".into()))
    }
}

impl SyncBackend for PgSyncBackend {
    fn apply_push(
        &self,
        request: &PushRequest,
        resolver: &dyn ConflictResolver,
    ) -> Result<PushResponse, SyncError> {
        let _ = (request, resolver);
        Err(SyncError::Backend("unimplemented".into()))
    }

    fn pull_since(&self, cursor: Version, limit: i64) -> Result<PullResponse, SyncError> {
        let _ = (cursor, limit);
        Err(SyncError::Backend("unimplemented".into()))
    }

    fn gc_tombstones(&self, up_to: Version) -> Result<u64, SyncError> {
        let _ = up_to;
        Err(SyncError::Backend("unimplemented".into()))
    }

    fn tombstone_horizon(&self) -> Result<Version, SyncError> {
        Err(SyncError::Backend("unimplemented".into()))
    }

    fn latest_version(&self) -> Result<Version, SyncError> {
        Err(SyncError::Backend("unimplemented".into()))
    }
}

/// Build the sync router (`POST /push`, `GET /pull`).
///
/// Generic over the host router's state so it mounts both on an Autumn app
/// (`AppBuilder::nest("/sync", router(...))` with
/// [`crate::AppState`]) and on a bare `axum::Router<()>` in tests. Mount it
/// behind authentication — the endpoints trust `device_id` as sent.
#[must_use]
pub fn router<S>(
    backend: Arc<dyn SyncBackend>,
    resolver: Arc<dyn ConflictResolver>,
) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let push_backend = Arc::clone(&backend);
    let push_resolver = Arc::clone(&resolver);
    axum::Router::new()
        .route(
            "/push",
            post(move |Json(request): Json<PushRequest>| {
                let backend = Arc::clone(&push_backend);
                let resolver = Arc::clone(&push_resolver);
                async move {
                    let result = tokio::task::spawn_blocking(move || {
                        backend.apply_push(&request, resolver.as_ref())
                    })
                    .await;
                    match result {
                        Ok(Ok(response)) => Json(response).into_response(),
                        Ok(Err(err)) => {
                            (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response()
                        }
                        Err(err) => {
                            (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response()
                        }
                    }
                }
            }),
        )
        .route(
            "/pull",
            get(move |Query(query): Query<PullQuery>| {
                let backend = Arc::clone(&backend);
                async move {
                    let result = tokio::task::spawn_blocking(move || {
                        backend.pull_since(query.cursor, query.limit)
                    })
                    .await;
                    match result {
                        Ok(Ok(response)) => Json(response).into_response(),
                        Ok(Err(err)) => {
                            (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response()
                        }
                        Err(err) => {
                            (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response()
                        }
                    }
                }
            }),
        )
}
