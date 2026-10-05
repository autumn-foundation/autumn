//! H11: add `Retry-After` to 503 responses.

use autumn_web::middleware::{AutumnErrorInfo, ExceptionFilter};
use axum::response::Response;

/// Adds `Retry-After` to each 503 that has none.
pub struct RetryAfterOn503;

impl ExceptionFilter for RetryAfterOn503 {
    fn filter(&self, _error: &AutumnErrorInfo, _response: Response) -> Response {
        todo!("H11")
    }
}
