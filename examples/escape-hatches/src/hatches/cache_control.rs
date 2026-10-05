//! H7: an app-wide `Cache-Control` header, as a tower layer on `.layer(..)`.
//!
//! Each page shows live stock counts. A shared cache (a CDN or an office
//! proxy) must not keep a copy. The framework sets `Cache-Control` only on
//! its own assets and endpoints. It has no app-wide setting. In a handler,
//! `etag::cache_for(..).no_store()` sets the header, but each route must
//! repeat it, and it does not reach error pages or raw routers.
//!
//! So the app adds `tower_http`'s `SetResponseHeaderLayer` with `.layer(..)`.
//! The layer sets the header on each response.
//!
//! `if_not_present` keeps a value that a route sets. So a route can still
//! opt in to caching.

use axum::http::{HeaderValue, header::CACHE_CONTROL};
use tower_http::set_header::SetResponseHeaderLayer;

/// `Cache-Control: no-store` on each response that has no `Cache-Control`.
#[must_use]
pub fn no_store() -> SetResponseHeaderLayer<HeaderValue> {
    SetResponseHeaderLayer::if_not_present(CACHE_CONTROL, HeaderValue::from_static("no-store"))
}
