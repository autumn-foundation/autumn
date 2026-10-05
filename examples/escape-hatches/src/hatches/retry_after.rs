//! H11: add `Retry-After` to 503 responses, with `.exception_filter(..)`.
//!
//! A 503 says "try again later". Scanners wait for the number of
//! seconds in `Retry-After`. The framework turns a Postgres statement
//! timeout into a 503 (`autumn.query_timeout`), but it sends no
//! `Retry-After`. Without it, clients retry at once and add load.
//!
//! An exception filter runs on every error response that the framework
//! makes. This one adds the header where it is missing.

use autumn_web::middleware::{AutumnErrorInfo, ExceptionFilter};
use axum::http::{HeaderValue, StatusCode, header::RETRY_AFTER};
use axum::response::Response;

/// The seconds that a client waits after a 503.
pub const RETRY_AFTER_SECS: &str = "2";

/// Adds `Retry-After` to each 503 that has none.
pub struct RetryAfterOn503;

impl ExceptionFilter for RetryAfterOn503 {
    fn filter(&self, _error: &AutumnErrorInfo, mut response: Response) -> Response {
        if response.status() == StatusCode::SERVICE_UNAVAILABLE
            && !response.headers().contains_key(RETRY_AFTER)
        {
            response
                .headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from_static(RETRY_AFTER_SECS));
        }
        response
    }
}
