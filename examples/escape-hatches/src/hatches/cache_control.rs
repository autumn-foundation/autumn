//! H7: an app-wide `Cache-Control` header.

use axum::http::HeaderValue;
use tower_http::set_header::SetResponseHeaderLayer;

/// `Cache-Control: no-store` on each response that has none.
#[must_use]
pub fn no_store() -> SetResponseHeaderLayer<HeaderValue> {
    todo!("H7")
}
