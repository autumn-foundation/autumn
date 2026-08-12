//! Wire-level recording of a request's database traffic.
//!
//! Stub: the recording pool, connection recorder and capture-eligibility
//! decision land in the GREEN commit of #1598.

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
    )
)]

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use diesel_async::AsyncPgConnection;
use diesel_async::pooled_connection::deadpool::Pool;

use crate::capsule::capture::CaptureScope;
use crate::config::{AutumnConfig, DatabaseConfig};
use crate::db::{DatabaseTopology, PoolError};

/// Boot-time pool factory shape shared with `App::run`'s pool provider slot.
pub type CapturePoolProvider = Box<
    dyn FnOnce(
            DatabaseConfig,
        )
            -> Pin<Box<dyn Future<Output = Result<Option<DatabaseTopology>, PoolError>> + Send>>
        + Send,
>;

/// Why a database URL cannot be recorded, or `None` when it can.
#[must_use]
pub fn capture_unavailable_reason(_url: &str) -> Option<String> {
    None
}

/// Copy the process-wide "DB capture is unavailable" note onto a scope.
pub fn note_db_capture_unavailable(_scope: &CaptureScope) {}

/// Build a pool whose connections tee their PostgreSQL wire traffic.
///
/// # Errors
///
/// Returns [`PoolError`] when the pool cannot be constructed.
pub fn build_recording_pool(
    url: &str,
    max_size: usize,
    connect_timeout: Duration,
) -> Result<Pool<AsyncPgConnection>, PoolError> {
    let manager =
        diesel_async::pooled_connection::AsyncDieselConnectionManager::<AsyncPgConnection>::new(
            url,
        );
    Ok(Pool::builder(manager)
        .max_size(max_size.max(1))
        .wait_timeout(Some(connect_timeout))
        .create_timeout(Some(connect_timeout))
        .runtime(deadpool::Runtime::Tokio1)
        .build()?)
}

/// Wrap `App::run`'s pool provider slot with the recording factory.
#[must_use]
pub fn maybe_capture_pool_provider(
    existing: Option<CapturePoolProvider>,
    _config: &AutumnConfig,
) -> Option<CapturePoolProvider> {
    existing
}
