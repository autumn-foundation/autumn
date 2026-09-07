//! Framework error type and result alias.
//!
//! [`AutumnError`] wraps any `Error + Send + Sync` with an HTTP status code.
//! The blanket [`From`] impl maps all errors to `500 Internal Server Error`,
//! so the `?` operator works in handlers with zero ceremony.
//!
//! For non-500 cases, use the status refinement constructors:
//!
//! - [`AutumnError::not_found`] -- 404
//! - [`AutumnError::bad_request`] -- 400
//! - [`AutumnError::unprocessable`] -- 422
//! - [`AutumnError::service_unavailable`] -- 503
//! - [`AutumnError::with_status`] -- arbitrary status code
//!
//! For simple string messages without wrapping an error type:
//!
//! - [`AutumnError::not_found_msg`] -- 404 with a message
//! - [`AutumnError::bad_request_msg`] -- 400 with a message
//! - [`AutumnError::unprocessable_msg`] -- 422 with a message
//! - [`AutumnError::service_unavailable_msg`] -- 503 with a message
//!
//! # Response format
//!
//! When an `AutumnError` is returned from a handler, it renders as JSON:
//!
//! ```json
//! { "error": { "status": 404, "message": "user not found" } }
//! ```
//!
//! # Examples
//!
//! ```rust
//! use autumn_web::error::AutumnError;
//! use http::StatusCode;
//!
//! // Blanket From impl: any Error becomes 500
//! let err: AutumnError = std::io::Error::other("disk full").into();
//! assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
//!
//! // Explicit status constructors
//! let err = AutumnError::not_found(std::io::Error::other("no such user"));
//! assert_eq!(err.status(), StatusCode::NOT_FOUND);
//! ```

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// Simple error type wrapping a string message.
///
/// Used by the `_msg` convenience constructors on [`AutumnError`] so callers
/// don't need to wrap strings in `std::io::Error`.
#[derive(Debug)]
struct StringError(String);

impl std::fmt::Display for StringError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StringError {}

/// JSON body for RFC 7807 Problem Details responses.
#[derive(Clone, Debug, Serialize)]
pub struct ProblemDetails {
    /// Problem type URI. Autumn uses stable `https://autumn.dev/problems/...`
    /// URIs for framework-generated errors.
    #[serde(rename = "type")]
    pub type_uri: String,
    /// Short human-readable title for the status/problem class.
    pub title: String,
    /// HTTP status code.
    pub status: u16,
    /// Client-safe human-readable explanation.
    pub detail: String,
    /// Request path or URI reference for the specific occurrence.
    pub instance: Option<String>,
    /// Stable machine-readable Autumn error code.
    pub code: String,
    /// Request ID for log correlation, when the request pipeline assigned one.
    pub request_id: Option<String>,
    /// Field-level validation failures. Empty for non-validation errors.
    pub errors: Vec<ProblemFieldError>,
}

/// Field-level validation detail in the Problem Details `errors` extension.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ProblemFieldError {
    /// Field name as seen by the request payload or form.
    pub field: String,
    /// Stable list of validation messages for this field.
    pub messages: Vec<String>,
}

/// Framework error type wrapping any error with an HTTP status code.
///
/// # Usage
///
/// The `?` operator converts any `std::error::Error` into an `AutumnError`
/// with status `500 Internal Server Error`:
///
/// ```rust,no_run
/// use autumn_web::prelude::*;
///
/// #[get("/")]
/// async fn handler() -> AutumnResult<&'static str> {
///     autumn_web::reexports::tokio::fs::read_to_string("missing.txt").await?; // becomes 500 on error
///     Ok("ok")
/// }
/// ```
///
/// For expected errors, use a status refinement constructor:
///
/// ```rust,no_run
/// use autumn_web::prelude::*;
///
/// #[get("/users/{id}")]
/// async fn get_user(axum::extract::Path(id): axum::extract::Path<i32>) -> AutumnResult<String> {
///     if id < 0 {
///         return Err(AutumnError::bad_request(
///             std::io::Error::other("id must be positive"),
///         ));
///     }
///     Ok(format!("user {id}"))
/// }
/// ```
///
/// # `Display`
///
/// `Display` prints the wrapped error's message. A validation error then
/// appends its fields, sorted by field name:
///
/// ```text
/// Validation failed: email: Must be a valid email address; title: Too short
/// ```
///
/// Fields are separated by `"; "` and a field's messages by `", "`. A field
/// with no messages is skipped. The messages are developer-authored, so do
/// not put untrusted text in them: `Display` output reaches logs.
///
/// [`message`](AutumnError::message) returns the wrapped error's message
/// alone. The response body renders that string, not this one, and redacts
/// it for a `5xx` outside a dev profile. Neither is redacted here.
///
/// # Why no `Error` impl
///
/// `AutumnError` intentionally does **not** implement [`std::error::Error`].
/// Doing so would conflict with the blanket `From<E: Error>` impl (the
/// reflexive `From<T> for T` would overlap). This type is a *response*
/// wrapper, not a propagatable error.
pub struct AutumnError {
    inner: Box<dyn std::error::Error + Send + Sync>,
    status: StatusCode,
    details: Option<std::collections::HashMap<String, Vec<String>>>,
    problem_type: Option<&'static str>,
    cache_idempotency_response: bool,
    /// Backtrace captured at error creation time in debug builds.
    /// Transferred to `AutumnErrorInfo` for the dev overlay.
    #[cfg(debug_assertions)]
    pub(crate) backtrace_string: Option<String>,
}

/// Convenience alias -- the standard return type for Autumn handlers.
///
/// Equivalent to `Result<T, AutumnError>`. Use this as the return type
/// for any handler that might fail.
///
/// # Examples
///
/// ```rust,no_run
/// use autumn_web::prelude::*;
///
/// #[get("/")]
/// async fn index() -> AutumnResult<&'static str> {
///     Ok("hello")
/// }
/// ```
pub type AutumnResult<T> = Result<T, AutumnError>;

impl<E> From<E> for AutumnError
where
    E: std::error::Error + Send + Sync + 'static,
{
    fn from(err: E) -> Self {
        // #2423: Postgres refuses an embedded NUL byte in a TEXT/VARCHAR column
        // (SQLSTATE 22021). That is malformed client input reaching the
        // database, not a server bug, so it takes the 422 a `#[validate(...)]`
        // rejection would have produced had a validator been able to see the
        // byte. Handled first and by returning, so this classification has one
        // unambiguous precedence rather than depending on where it sits among
        // the status overrides below.
        //
        // The error is re-wrapped rather than merely restatused: the 422 page
        // renders the message verbatim where the 500 page redacts it, so
        // downgrading alone would put a raw Postgres message on screen. The
        // original stays reachable as `source()`.
        #[cfg(feature = "db")]
        if error_chain_is_pg_nul_rejection(&err) {
            return Self {
                inner: Box::new(NulByteRejected(err)),
                status: StatusCode::UNPROCESSABLE_ENTITY,
                details: None,
                problem_type: None,
                cache_idempotency_response: false,
                #[cfg(debug_assertions)]
                backtrace_string: Some(format!("{}", std::backtrace::Backtrace::force_capture())),
            };
        }

        let mut status = StatusCode::INTERNAL_SERVER_ERROR;
        let any_err: &dyn std::any::Any = &err;

        if std::any::type_name::<E>().contains("CircuitBreakerError")
            && err.to_string() == "circuit breaker is open"
        {
            status = StatusCode::SERVICE_UNAVAILABLE;
        }

        #[cfg(feature = "http-client")]
        {
            if matches!(
                any_err.downcast_ref::<crate::http_client::ClientError>(),
                Some(crate::http_client::ClientError::CircuitBreakerOpen)
            ) {
                status = StatusCode::SERVICE_UNAVAILABLE;
            }
        }

        // Web Push (#1392) distinguishes client-fault failures (a malformed
        // browser subscription, an endpoint already claimed) from server-fault
        // ones, so an app calling `push.subscribe(…).await?` from its own
        // handler gets the same status the built-in push router would return
        // rather than a blanket 500.
        if let Some(push_err) = any_err.downcast_ref::<crate::push::PushError>() {
            status = push_err.status();
        }

        if matches!(
            any_err.downcast_ref::<crate::lock::LockError>(),
            Some(
                crate::lock::LockError::PoolUnavailable(_) | crate::lock::LockError::Timeout { .. }
            )
        ) {
            status = StatusCode::SERVICE_UNAVAILABLE;
        }

        #[cfg(feature = "mail")]
        {
            if let Some(crate::mail::MailError::RuntimeUnavailable(msg)) =
                any_err.downcast_ref::<crate::mail::MailError>()
                && msg.contains("circuit breaker is open")
            {
                status = StatusCode::SERVICE_UNAVAILABLE;
            }
        }

        // A per-tenant memory quota breach is a soft, retryable resource limit,
        // so it maps to 503 Service Unavailable rather than a 500.
        if any_err
            .downcast_ref::<crate::tenant_cell::QuotaExceeded>()
            .is_some()
        {
            status = StatusCode::SERVICE_UNAVAILABLE;
        }

        Self {
            inner: Box::new(err),
            status,
            details: None,
            problem_type: None,
            cache_idempotency_response: false,
            #[cfg(debug_assertions)]
            backtrace_string: Some(format!("{}", std::backtrace::Backtrace::force_capture())),
        }
    }
}

impl AutumnError {
    /// Override the HTTP status code.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use autumn_web::error::AutumnError;
    /// use http::StatusCode;
    ///
    /// let err: AutumnError = std::io::Error::other("forbidden").into();
    /// let err = err.with_status(StatusCode::FORBIDDEN);
    /// assert_eq!(err.status(), StatusCode::FORBIDDEN);
    /// ```
    #[must_use]
    pub const fn with_status(mut self, status: StatusCode) -> Self {
        self.status = status;
        self
    }

    /// Create a `500 Internal Server Error`.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use autumn_web::error::AutumnError;
    /// use http::StatusCode;
    ///
    /// let err = AutumnError::internal_server_error(std::io::Error::other("boom"));
    /// assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
    /// ```
    pub fn internal_server_error(err: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self {
            inner: Box::new(err),
            status: StatusCode::INTERNAL_SERVER_ERROR,
            details: None,
            problem_type: None,
            cache_idempotency_response: false,
            #[cfg(debug_assertions)]
            backtrace_string: Some(format!("{}", std::backtrace::Backtrace::force_capture())),
        }
    }

    /// Create a `404 Not Found` error.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use autumn_web::error::AutumnError;
    /// use http::StatusCode;
    ///
    /// let err = AutumnError::not_found(std::io::Error::other("no such user"));
    /// assert_eq!(err.status(), StatusCode::NOT_FOUND);
    /// ```
    pub fn not_found(err: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self {
            inner: Box::new(err),
            status: StatusCode::NOT_FOUND,
            details: None,
            problem_type: None,
            cache_idempotency_response: false,
            #[cfg(debug_assertions)]
            backtrace_string: Some(format!("{}", std::backtrace::Backtrace::force_capture())),
        }
    }

    /// Create a `400 Bad Request` error.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use autumn_web::error::AutumnError;
    /// use http::StatusCode;
    ///
    /// let err = AutumnError::bad_request(std::io::Error::other("invalid input"));
    /// assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    /// ```
    pub fn bad_request(err: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self {
            inner: Box::new(err),
            status: StatusCode::BAD_REQUEST,
            details: None,
            problem_type: None,
            cache_idempotency_response: false,
            #[cfg(debug_assertions)]
            backtrace_string: Some(format!("{}", std::backtrace::Backtrace::force_capture())),
        }
    }

    /// Create a `422 Unprocessable Entity` error.
    ///
    /// Use this for validation failures where the request is syntactically
    /// valid but semantically incorrect.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use autumn_web::error::AutumnError;
    /// use http::StatusCode;
    ///
    /// let err = AutumnError::unprocessable(std::io::Error::other("age must be positive"));
    /// assert_eq!(err.status(), StatusCode::UNPROCESSABLE_ENTITY);
    /// ```
    pub fn unprocessable(err: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self {
            inner: Box::new(err),
            status: StatusCode::UNPROCESSABLE_ENTITY,
            details: None,
            problem_type: None,
            cache_idempotency_response: false,
            #[cfg(debug_assertions)]
            backtrace_string: Some(format!("{}", std::backtrace::Backtrace::force_capture())),
        }
    }

    /// Create a `503 Service Unavailable` error.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use autumn_web::error::AutumnError;
    /// use http::StatusCode;
    ///
    /// let err = AutumnError::service_unavailable(std::io::Error::other("pool exhausted"));
    /// assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
    /// ```
    pub fn service_unavailable(err: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self {
            inner: Box::new(err),
            status: StatusCode::SERVICE_UNAVAILABLE,
            details: None,
            problem_type: None,
            cache_idempotency_response: false,
            #[cfg(debug_assertions)]
            backtrace_string: Some(format!("{}", std::backtrace::Backtrace::force_capture())),
        }
    }

    /// Create a `401 Unauthorized` error.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use autumn_web::error::AutumnError;
    /// use http::StatusCode;
    ///
    /// let err = AutumnError::unauthorized(std::io::Error::other("not logged in"));
    /// assert_eq!(err.status(), StatusCode::UNAUTHORIZED);
    /// ```
    pub fn unauthorized(err: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self {
            inner: Box::new(err),
            status: StatusCode::UNAUTHORIZED,
            details: None,
            problem_type: None,
            cache_idempotency_response: false,
            #[cfg(debug_assertions)]
            backtrace_string: Some(format!("{}", std::backtrace::Backtrace::force_capture())),
        }
    }

    /// Create a `403 Forbidden` error.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use autumn_web::error::AutumnError;
    /// use http::StatusCode;
    ///
    /// let err = AutumnError::forbidden(std::io::Error::other("not allowed"));
    /// assert_eq!(err.status(), StatusCode::FORBIDDEN);
    /// ```
    pub fn forbidden(err: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self {
            inner: Box::new(err),
            status: StatusCode::FORBIDDEN,
            details: None,
            problem_type: None,
            cache_idempotency_response: false,
            #[cfg(debug_assertions)]
            backtrace_string: Some(format!("{}", std::backtrace::Backtrace::force_capture())),
        }
    }

    /// Create a `422 Unprocessable Entity` error with field-level
    /// validation details.
    ///
    /// Use this when a request fails multiple field-specific validation rules
    /// (e.g., in a form submission). It attaches the `details` parameter, a mapping
    /// of field names to their respective error messages, so the client can display
    /// errors next to the relevant inputs.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use autumn_web::error::AutumnError;
    /// use http::StatusCode;
    /// use std::collections::HashMap;
    ///
    /// let mut errors = HashMap::new();
    /// errors.insert("username".to_string(), vec!["Username is taken".to_string()]);
    ///
    /// let err = AutumnError::validation(errors);
    /// assert_eq!(err.status(), StatusCode::UNPROCESSABLE_ENTITY);
    /// ```
    #[must_use]
    pub fn validation(details: std::collections::HashMap<String, Vec<String>>) -> Self {
        Self {
            inner: Box::new(StringError("Validation failed".into())),
            status: StatusCode::UNPROCESSABLE_ENTITY,
            details: Some(details),
            problem_type: None,
            cache_idempotency_response: false,
            #[cfg(debug_assertions)]
            backtrace_string: Some(format!("{}", std::backtrace::Backtrace::force_capture())),
        }
    }

    // ── String-message convenience constructors ────────────────

    /// Create a `500 Internal Server Error` from a plain string message.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use autumn_web::error::AutumnError;
    /// use http::StatusCode;
    ///
    /// let err = AutumnError::internal_server_error_msg("Database explosion");
    /// assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
    /// ```
    pub fn internal_server_error_msg(msg: impl Into<String>) -> Self {
        Self::internal_server_error(StringError(msg.into()))
    }

    /// Create a `404 Not Found` error from a plain string message.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use autumn_web::error::AutumnError;
    /// use http::StatusCode;
    ///
    /// let err = AutumnError::not_found_msg("No such user");
    /// assert_eq!(err.status(), StatusCode::NOT_FOUND);
    /// assert_eq!(err.to_string(), "No such user");
    /// ```
    pub fn not_found_msg(msg: impl Into<String>) -> Self {
        Self::not_found(StringError(msg.into()))
    }

    /// Create a `400 Bad Request` error from a plain string message.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use autumn_web::error::AutumnError;
    /// use http::StatusCode;
    ///
    /// let err = AutumnError::bad_request_msg("Invalid input parameter");
    /// assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    /// ```
    pub fn bad_request_msg(msg: impl Into<String>) -> Self {
        Self::bad_request(StringError(msg.into()))
    }

    /// Create a `422 Unprocessable Entity` error from a plain string message.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use autumn_web::error::AutumnError;
    /// use http::StatusCode;
    ///
    /// let err = AutumnError::unprocessable_msg("Title is required");
    /// assert_eq!(err.status(), StatusCode::UNPROCESSABLE_ENTITY);
    /// ```
    pub fn unprocessable_msg(msg: impl Into<String>) -> Self {
        Self::unprocessable(StringError(msg.into()))
    }

    /// Create a `401 Unauthorized` error from a plain string message.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use autumn_web::error::AutumnError;
    /// use http::StatusCode;
    ///
    /// let err = AutumnError::unauthorized_msg("Please log in to continue");
    /// assert_eq!(err.status(), StatusCode::UNAUTHORIZED);
    /// ```
    pub fn unauthorized_msg(msg: impl Into<String>) -> Self {
        Self::unauthorized(StringError(msg.into()))
    }

    /// Create a `403 Forbidden` error from a plain string message.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use autumn_web::error::AutumnError;
    /// use http::StatusCode;
    ///
    /// let err = AutumnError::forbidden_msg("You lack admin privileges");
    /// assert_eq!(err.status(), StatusCode::FORBIDDEN);
    /// ```
    pub fn forbidden_msg(msg: impl Into<String>) -> Self {
        Self::forbidden(StringError(msg.into()))
    }

    /// Create a `503 Service Unavailable` error from a plain string message.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use autumn_web::error::AutumnError;
    /// use http::StatusCode;
    ///
    /// let err = AutumnError::service_unavailable_msg("Database connection pool exhausted");
    /// assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
    /// ```
    pub fn service_unavailable_msg(msg: impl Into<String>) -> Self {
        Self::service_unavailable(StringError(msg.into()))
    }

    /// Create a `409 Conflict` error.
    ///
    /// Use this for optimistic-lock conflicts surfaced by repository `update`
    /// calls when the client's expected version is stale.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use autumn_web::error::AutumnError;
    /// use http::StatusCode;
    ///
    /// let err = AutumnError::conflict(std::io::Error::other("stale version"));
    /// assert_eq!(err.status(), StatusCode::CONFLICT);
    /// ```
    pub fn conflict(err: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self {
            inner: Box::new(err),
            status: StatusCode::CONFLICT,
            details: None,
            problem_type: Some(PROBLEM_TYPE_CONFLICT),
            cache_idempotency_response: false,
            #[cfg(debug_assertions)]
            backtrace_string: Some(format!("{}", std::backtrace::Backtrace::force_capture())),
        }
    }

    /// Create a `409 Conflict` error from a plain string message.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use autumn_web::error::AutumnError;
    /// use http::StatusCode;
    ///
    /// let err = AutumnError::conflict_msg("Concurrent edit: please reload and retry");
    /// assert_eq!(err.status(), StatusCode::CONFLICT);
    /// ```
    pub fn conflict_msg(msg: impl Into<String>) -> Self {
        Self::conflict(StringError(msg.into()))
    }

    /// Create a `410 Gone` error.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use autumn_web::error::AutumnError;
    /// use http::StatusCode;
    ///
    /// let err = AutumnError::gone(std::io::Error::other("sunsetted"));
    /// assert_eq!(err.status(), StatusCode::GONE);
    /// ```
    pub fn gone(err: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self {
            inner: Box::new(err),
            status: StatusCode::GONE,
            details: None,
            problem_type: Some(PROBLEM_TYPE_GONE),
            cache_idempotency_response: false,
            #[cfg(debug_assertions)]
            backtrace_string: Some(format!("{}", std::backtrace::Backtrace::force_capture())),
        }
    }

    /// Create a `410 Gone` error from a plain string message.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use autumn_web::error::AutumnError;
    /// use http::StatusCode;
    ///
    /// let err = AutumnError::gone_msg("API version has been sunsetted");
    /// assert_eq!(err.status(), StatusCode::GONE);
    /// ```
    pub fn gone_msg(msg: impl Into<String>) -> Self {
        Self::gone(StringError(msg.into()))
    }

    /// Create a `503 Service Unavailable` error indicating that a database
    /// query was cancelled due to a statement timeout (Postgres `57014`).
    ///
    /// The problem details payload carries `"autumn.query_timeout"` as the
    /// machine-readable code, which allows clients to distinguish a transient
    /// timeout from other 503 conditions and apply appropriate retry logic.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use autumn_web::error::AutumnError;
    /// use http::StatusCode;
    ///
    /// let err = AutumnError::query_timeout("query exceeded statement_timeout");
    /// assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
    /// ```
    pub fn query_timeout(msg: impl Into<String>) -> Self {
        Self {
            inner: Box::new(StringError(msg.into())),
            status: StatusCode::SERVICE_UNAVAILABLE,
            details: None,
            problem_type: Some(PROBLEM_TYPE_QUERY_TIMEOUT),
            cache_idempotency_response: false,
            #[cfg(debug_assertions)]
            backtrace_string: Some(format!("{}", std::backtrace::Backtrace::force_capture())),
        }
    }

    /// Returns the HTTP status code associated with this error.
    ///
    /// This is the status the error was assigned. The response reclassifies
    /// a cancelled database statement to `503`, so [`code`](Self::code) can
    /// report `autumn.query_timeout` while this still reports the assigned
    /// status.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use autumn_web::error::AutumnError;
    /// use http::StatusCode;
    ///
    /// let err: AutumnError = std::io::Error::other("boom").into();
    /// assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
    /// ```
    #[must_use]
    pub const fn status(&self) -> StatusCode {
        self.status
    }

    /// Returns the validation messages, keyed by field name.
    ///
    /// The map is `Some` for [`AutumnError::validation`] and for a failed
    /// [`ValidateExt::validate`](crate::ValidateExt::validate). It is `None`
    /// for every other error. Read it where there is no HTTP response to
    /// parse.
    ///
    /// The map is a [`HashMap`](std::collections::HashMap), so its iteration
    /// order is unspecified. Sort the keys before you render them.
    ///
    /// An empty map is still `Some`. [`code`](Self::code) then reports the
    /// status code, not `autumn.validation_failed`.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use autumn_web::error::AutumnError;
    /// use std::collections::HashMap;
    ///
    /// let mut errors = HashMap::new();
    /// errors.insert("email".to_string(), vec!["Invalid".to_string()]);
    ///
    /// let err = AutumnError::validation(errors);
    /// assert_eq!(err.details().unwrap()["email"], ["Invalid".to_string()]);
    /// ```
    #[must_use]
    pub const fn details(&self) -> Option<&std::collections::HashMap<String, Vec<String>>> {
        self.details.as_ref()
    }

    /// The wrapped error's message, without the validation fields.
    ///
    /// [`Display`](std::fmt::Display) appends the failing fields on top of
    /// this and is for a human reader. Use `message` where the string is
    /// stored, broadcast, or compared across versions.
    ///
    /// **Not redacted.** For a `4xx` this is the `application/problem+json`
    /// `detail`, but a `5xx` response outside a dev profile replaces that
    /// `detail` with a generic line, and this still returns the wrapped
    /// error — a database or infrastructure message. Check
    /// [`status`](Self::status) before you send it to a client.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use autumn_web::error::AutumnError;
    /// use std::collections::HashMap;
    ///
    /// let mut errors = HashMap::new();
    /// errors.insert("email".to_string(), vec!["Invalid".to_string()]);
    ///
    /// let err = AutumnError::validation(errors);
    /// assert_eq!(err.message(), "Validation failed");
    /// assert_eq!(err.to_string(), "Validation failed: email: Invalid");
    /// ```
    #[must_use]
    pub fn message(&self) -> String {
        self.inner.to_string()
    }

    /// The stable problem code the rendered response carries.
    ///
    /// Same value as the `code` member of the `application/problem+json`
    /// body — both come from one derivation — so a non-HTTP caller can
    /// branch on it without building a response. It is borrowed for every
    /// code the framework names today.
    ///
    /// A cancelled database statement is reclassified here as it is in the
    /// response, so this can read `autumn.query_timeout` where
    /// [`status`](Self::status) still reads the assigned status. See that
    /// method.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use autumn_web::error::AutumnError;
    ///
    /// assert_eq!(AutumnError::not_found_msg("no such user").code(), "autumn.not_found");
    /// assert_eq!(AutumnError::gone_msg("sunsetted").code(), "autumn.gone");
    /// ```
    #[must_use]
    pub fn code(&self) -> std::borrow::Cow<'static, str> {
        let (status, problem_type) = self.rendered_problem();
        problem_code(status, self.has_field_errors(), problem_type)
    }

    /// Whether this error names at least one field.
    ///
    /// A field with an empty message list still counts, so this matches the
    /// `has_validation_errors` test the response body uses.
    fn has_field_errors(&self) -> bool {
        self.details.as_ref().is_some_and(|map| !map.is_empty())
    }

    /// Status and problem type the rendered response uses.
    ///
    /// A cancelled database statement arrives as a plain `500`; the response
    /// demotes it to a `503` query timeout, so [`code`](Self::code) reads the
    /// same classification the body shows. Reads the wrapped error, never
    /// `Display`, so a validation message cannot reclassify a `422`.
    fn rendered_problem(&self) -> (StatusCode, Option<&'static str>) {
        let lowered = self.inner.to_string().to_lowercase();
        if lowered.contains("57014")
            || lowered.contains("query_canceled")
            || lowered.contains("canceling statement due to statement timeout")
            || lowered.contains("statement timeout")
            || lowered.contains("query canceled")
        {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Some(PROBLEM_TYPE_QUERY_TIMEOUT),
            );
        }
        (self.status, self.problem_type)
    }

    #[doc(hidden)]
    #[must_use]
    pub(crate) const fn cache_idempotency_response(mut self) -> Self {
        self.cache_idempotency_response = true;
        self
    }

    /// Return the wrapped error's source chain as displayable messages.
    ///
    /// [`message`](Self::message) already gives the wrapped error's own
    /// message, so this list starts at that error's first source.
    #[must_use]
    pub fn source_chain(&self) -> Vec<String> {
        let mut chain = Vec::new();
        let mut source = self.inner.source();
        while let Some(error) = source {
            chain.push(error.to_string());
            source = error.source();
        }
        chain
    }

    /// Try to downcast the inner error to a specific type.
    #[must_use]
    pub fn downcast_ref<T: std::error::Error + 'static>(&self) -> Option<&T> {
        let err: &(dyn std::error::Error + 'static) = self.inner.as_ref();
        err.downcast_ref::<T>()
    }

    /// Try to downcast the inner error, or any error in its `source()` chain,
    /// to a specific type.
    ///
    /// Unlike [`downcast_ref`](Self::downcast_ref), which only inspects the
    /// top-level wrapped error, this walks the full chain — useful when a
    /// custom error type wraps a lower-level error (e.g. via
    /// `#[source]`/`#[from]`) without itself being that type.
    #[must_use]
    pub fn downcast_chain_ref<T: std::error::Error + 'static>(&self) -> Option<&T> {
        let mut current: Option<&(dyn std::error::Error + 'static)> = Some(self.inner.as_ref());
        while let Some(err) = current {
            if let Some(t) = err.downcast_ref::<T>() {
                return Some(t);
            }
            current = err.source();
        }
        None
    }
}

/// Checks whether `err`'s inner error is a Postgres unique-constraint
/// violation (SQLSTATE `23505`) matching `mapping` (issue #1032).
///
/// If `err` is a unique-violation whose constraint name matches one of
/// `mapping`'s entries, returns the offending field name and the message to
/// surface inline. `mapping` pairs each unique index/constraint name (e.g.
/// `idx_users_email_unique` — see `autumn generate`'s
/// `schema_edit::unique_index_sql`) with the form field name and message to
/// show when that constraint is violated.
///
/// Returns `None` for any other error, or for an unrecognized constraint
/// name — both cases the caller should propagate normally (`?`/[`From`]),
/// which falls through to the blanket `500` mapping. This is the single
/// shared place this classification happens; generated `create`/`update`
/// handlers call it instead of hand-rolling a `DatabaseErrorKind` match
/// per scaffold.
///
/// Works whether `err` wraps a raw `diesel::result::Error` directly (a bare
/// `.execute(...).await?`) or one already converted by a generated
/// repository method (`repo.save(...).await?`) — both route the original
/// diesel error through the `?` operator's blanket [`From`] impl, so
/// [`AutumnError::downcast_ref`] recovers it either way.
///
/// # Examples
///
/// ```rust
/// use autumn_web::error::{AutumnError, unique_violation_field};
///
/// let err = AutumnError::internal_server_error_msg("not a db error");
/// assert_eq!(unique_violation_field(&err, &[("idx_users_email_unique", "email", "taken")]), None);
/// ```
#[cfg(feature = "db")]
#[must_use]
pub fn unique_violation_field<'a>(
    err: &AutumnError,
    mapping: &'a [(&str, &str, &str)],
) -> Option<(&'a str, &'a str)> {
    let diesel::result::Error::DatabaseError(
        diesel::result::DatabaseErrorKind::UniqueViolation,
        info,
    ) = err.downcast_ref::<diesel::result::Error>()?
    else {
        return None;
    };
    let constraint = info.constraint_name()?;
    mapping
        .iter()
        .find(|(c, _, _)| *c == constraint)
        .map(|(_, field, message)| (*field, *message))
}

// ── #2423: Postgres refuses a NUL byte in TEXT — that is client input ──────

/// The client-facing message substituted for a Postgres NUL-byte rejection.
///
/// The raw server message (`invalid byte sequence for encoding "UTF8": 0x00`)
/// is a database internal. It must not become the visible message, because the
/// `422` error page renders `message` verbatim where the `500` page
/// deliberately does not — see `error_pages::defaults`' `render_422` and
/// `render_500`. Downgrading the status alone would therefore move a database
/// message onto a page that shows it.
pub const NUL_BYTE_REJECTED_MESSAGE: &str =
    "The submitted text contains a NUL character (0x00), which cannot be stored.";

/// Wrapper that gives a classified NUL rejection [`NUL_BYTE_REJECTED_MESSAGE`]
/// as its `Display` while keeping the original error as its `source()`, so
/// logs, error reporting and [`AutumnError::downcast_chain_ref`] still reach
/// the underlying diesel error.
#[cfg(feature = "db")]
#[derive(Debug)]
pub(crate) struct NulByteRejected<E>(E);

#[cfg(feature = "db")]
impl<E> std::fmt::Display for NulByteRejected<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(NUL_BYTE_REJECTED_MESSAGE)
    }
}

#[cfg(feature = "db")]
impl<E: std::error::Error + 'static> std::error::Error for NulByteRejected<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

/// Whether `err` carries a Postgres rejection of an embedded NUL byte
/// (SQLSTATE `22021`, issue #2423) — malformed *client* input rather than a
/// server bug.
///
/// A `TEXT`/`VARCHAR` column cannot hold `0x00`, so a value carrying one is
/// refused at `INSERT`/`UPDATE` time no matter how it arrived. The blanket
/// [`From`] impl already downgrades such an error to
/// `422 Unprocessable Entity`; this predicate is for handlers that want to go
/// further and fold it back into a form as a field error, the way
/// [`unique_violation_field`] is used for a uniqueness clash.
///
/// Walks the whole `source()` chain — the same walk the [`From`] impl makes,
/// through the one shared helper, so the two can never disagree about whether
/// a given error is this one.
///
/// Prefer preventing it: [`crate::form::ChangesetForm`] already rejects a NUL
/// at the form boundary with [`crate::form::NUL_CHARACTER_FIELD_ERROR`], so
/// this only fires for the paths no form extractor sees — a JSON API body, a
/// hand-written query, a background job.
///
/// # Examples
///
/// ```rust
/// use autumn_web::error::{AutumnError, is_nul_byte_violation};
///
/// let err = AutumnError::internal_server_error_msg("not a db error");
/// assert!(!is_nul_byte_violation(&err));
/// ```
#[cfg(feature = "db")]
#[must_use]
pub fn is_nul_byte_violation(err: &AutumnError) -> bool {
    error_chain_is_pg_nul_rejection(err.inner.as_ref())
}

/// Whether `err`, or anything in its `source()` chain, is Postgres refusing an
/// embedded NUL byte.
///
/// The single definition of "this was the client's byte, not our bug", shared
/// by [`is_nul_byte_violation`] and the blanket [`From`] impl. The chain walk
/// matters because a repository or service may wrap the diesel error in its own
/// type, and it continues past a non-matching diesel error rather than stopping
/// at the first one found.
#[cfg(feature = "db")]
pub(crate) fn error_chain_is_pg_nul_rejection(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(err);
    while let Some(cause) = current {
        if cause
            .downcast_ref::<diesel::result::Error>()
            .is_some_and(is_pg_nul_byte_error)
        {
            return true;
        }
        current = cause.source();
    }
    false
}

/// Whether a raw diesel error is Postgres refusing an embedded NUL byte.
#[cfg(feature = "db")]
fn is_pg_nul_byte_error(err: &diesel::result::Error) -> bool {
    let diesel::result::Error::DatabaseError(_, info) = err else {
        return false;
    };
    pg_message_is_nul_rejection(info.message())
}

/// Classify a Postgres server message as the `22021` NUL rejection.
///
/// Message matching, not SQLSTATE matching, because the code is unavailable:
/// `diesel-async`'s `pg/error_helper.rs` maps every SQLSTATE it does not
/// special-case (`22021` among them) to `DatabaseErrorKind::Unknown` and keeps
/// no copy of the code, and diesel's `DatabaseErrorInformation` trait exposes no
/// accessor for it.
///
/// **Anchored on purpose.** Postgres echoes submitted text into the primary
/// message of other errors (`invalid input syntax for type integer: "…"`,
/// `column "…" does not exist`), so a substring search for `0x00` is
/// attacker-satisfiable: a client that submits the right literal could get a
/// genuine server fault relabelled as its own fault — hidden from 5xx alerting
/// and rendered on the 422 page. Requiring the message to *end* with `: 0x00`
/// defeats that, because an echoed value is always followed by a closing quote
/// or trailing prose. `UTF` must appear too, and always does: `tokio-postgres`
/// fixes `client_encoding` to `UTF8`, so the encoding the server names in this
/// message is `UTF8` on every connection this framework opens.
///
/// The failure this leaves is a locale whose translation moves the byte literal
/// off the end of the message — the error then keeps its `500`, which is
/// exactly the pre-#2423 behavior. Failing back to "server bug" is the safe
/// direction; failing forward to "client's fault" is not.
#[cfg(feature = "db")]
fn pg_message_is_nul_rejection(message: &str) -> bool {
    message.ends_with(": 0x00") && message.contains("UTF")
}

impl std::fmt::Display for AutumnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.inner)?;

        // Fold the field map in, so `err.to_string()` says which field failed
        // off the HTTP path. Sorted, so the rendering is stable across runs.
        // The problem+json `detail` renders the wrapped error alone and is
        // unaffected.
        let mut fields: Vec<_> = self
            .details
            .iter()
            .flatten()
            .filter(|(_, messages)| !messages.is_empty())
            .collect();
        fields.sort_by_key(|(left, _)| *left);

        for (index, (field, messages)) in fields.into_iter().enumerate() {
            f.write_str(if index == 0 { ": " } else { "; " })?;
            write!(f, "{field}: ")?;
            for (position, message) in messages.iter().enumerate() {
                if position > 0 {
                    f.write_str(", ")?;
                }
                f.write_str(message)?;
            }
        }
        Ok(())
    }
}

impl std::fmt::Debug for AutumnError {
    #[allow(clippy::missing_fields_in_debug)]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutumnError")
            .field("status", &self.status)
            .field("inner", &self.inner)
            .field("details", &self.details)
            .field("problem_type", &self.problem_type)
            .field(
                "cache_idempotency_response",
                &self.cache_idempotency_response,
            )
            .finish_non_exhaustive()
    }
}

impl ProblemDetails {
    /// Build a Problem Details payload from framework error metadata.
    #[must_use]
    pub fn new(
        status: StatusCode,
        detail: impl Into<String>,
        details: Option<&std::collections::HashMap<String, Vec<String>>>,
    ) -> Self {
        problem_details(status, detail.into(), details, None, None, None, true)
    }
}

/// Build the canonical Problem Details payload.
#[must_use]
pub(crate) fn problem_details(
    status: StatusCode,
    detail: String,
    details: Option<&std::collections::HashMap<String, Vec<String>>>,
    explicit_type: Option<&'static str>,
    request_id: Option<String>,
    instance: Option<String>,
    expose_internal_detail: bool,
) -> ProblemDetails {
    let has_validation_errors = details.is_some_and(|map| !map.is_empty());
    let safe_detail = if status.is_server_error() && !expose_internal_detail {
        server_error_detail(status)
    } else {
        detail
    };

    let code = problem_code(status, has_validation_errors, explicit_type).into_owned();

    ProblemDetails {
        type_uri: explicit_type
            .unwrap_or_else(|| problem_type_for(status, has_validation_errors))
            .to_owned(),
        title: problem_title_for(status, has_validation_errors).to_owned(),
        status: status.as_u16(),
        detail: safe_detail,
        instance,
        code,
        request_id,
        errors: validation_errors(details),
    }
}

/// Serialize a Problem Details payload for middleware that cannot return
/// `axum::Json` directly because its response body type is generic.
#[must_use]
pub(crate) fn problem_details_json_string(
    status: StatusCode,
    detail: impl Into<String>,
    details: Option<&std::collections::HashMap<String, Vec<String>>>,
    explicit_type: Option<&'static str>,
    request_id: Option<String>,
    instance: Option<String>,
    expose_internal_detail: bool,
) -> String {
    let problem = problem_details(
        status,
        detail.into(),
        details,
        explicit_type,
        request_id,
        instance,
        expose_internal_detail,
    );
    problem_details_to_json_string(&problem)
}

/// Serialize an already-built Problem Details payload.
#[must_use]
pub(crate) fn problem_details_to_json_string(problem: &ProblemDetails) -> String {
    serde_json::to_string(&problem).unwrap_or_else(|_| {
        r#"{"type":"https://autumn.dev/problems/internal-server-error","title":"Internal Server Error","status":500,"detail":"Internal server error","instance":null,"code":"autumn.internal_server_error","request_id":null,"errors":[]}"#.to_owned()
    })
}

fn validation_errors(
    details: Option<&std::collections::HashMap<String, Vec<String>>>,
) -> Vec<ProblemFieldError> {
    let mut errors: Vec<_> = details
        .into_iter()
        .flat_map(std::collections::HashMap::iter)
        .map(|(field, messages)| ProblemFieldError {
            field: field.clone(),
            messages: messages.clone(),
        })
        .collect();
    errors.sort_by(|left, right| left.field.cmp(&right.field));
    errors
}

// Problem-type URIs Autumn attaches explicitly, overriding the one the status
// alone would select.
const PROBLEM_TYPE_CONFLICT: &str = "https://autumn.dev/problems/conflict";
const PROBLEM_TYPE_GONE: &str = "https://autumn.dev/problems/gone";
const PROBLEM_TYPE_QUERY_TIMEOUT: &str = "https://autumn.dev/problems/query-timeout";

/// The machine-readable code for a rendered problem.
///
/// One derivation for both the response body and [`AutumnError::code`], so
/// the two cannot disagree. An explicit type names the code through its last
/// path segment (hyphens to underscores, `autumn.` prefix); otherwise the
/// status and the presence of field errors do.
///
/// `"https://autumn.dev/problems/query-timeout"` → `"autumn.query_timeout"`.
fn problem_code(
    status: StatusCode,
    has_validation_errors: bool,
    explicit_type: Option<&str>,
) -> std::borrow::Cow<'static, str> {
    explicit_type.map_or_else(
        || std::borrow::Cow::Borrowed(problem_code_for(status, has_validation_errors)),
        |type_uri| {
            let slug = type_uri.rsplit('/').next().unwrap_or(type_uri);
            std::borrow::Cow::Owned(format!("autumn.{}", slug.replace('-', "_")))
        },
    )
}

const fn problem_type_for(status: StatusCode, has_validation_errors: bool) -> &'static str {
    if has_validation_errors {
        return "https://autumn.dev/problems/validation-failed";
    }

    match status {
        StatusCode::BAD_REQUEST => "https://autumn.dev/problems/bad-request",
        StatusCode::UNAUTHORIZED => "https://autumn.dev/problems/unauthorized",
        StatusCode::FORBIDDEN => "https://autumn.dev/problems/forbidden",
        StatusCode::NOT_FOUND => "https://autumn.dev/problems/not-found",
        StatusCode::GONE => "https://autumn.dev/problems/gone",
        StatusCode::CONFLICT => "https://autumn.dev/problems/conflict",
        StatusCode::PAYLOAD_TOO_LARGE => "https://autumn.dev/problems/payload-too-large",
        StatusCode::UNPROCESSABLE_ENTITY => "https://autumn.dev/problems/unprocessable-entity",
        StatusCode::INTERNAL_SERVER_ERROR => "https://autumn.dev/problems/internal-server-error",
        StatusCode::NOT_IMPLEMENTED => "https://autumn.dev/problems/not-implemented",
        StatusCode::SERVICE_UNAVAILABLE => "https://autumn.dev/problems/service-unavailable",
        _ => "about:blank",
    }
}

fn problem_title_for(status: StatusCode, has_validation_errors: bool) -> &'static str {
    if has_validation_errors {
        return "Validation Failed";
    }

    match status {
        StatusCode::BAD_REQUEST => "Bad Request",
        StatusCode::UNAUTHORIZED => "Unauthorized",
        StatusCode::FORBIDDEN => "Forbidden",
        StatusCode::NOT_FOUND => "Not Found",
        StatusCode::GONE => "Gone",
        StatusCode::CONFLICT => "Conflict",
        StatusCode::PAYLOAD_TOO_LARGE => "Payload Too Large",
        StatusCode::UNPROCESSABLE_ENTITY => "Unprocessable Entity",
        StatusCode::INTERNAL_SERVER_ERROR => "Internal Server Error",
        StatusCode::NOT_IMPLEMENTED => "Not Implemented",
        StatusCode::SERVICE_UNAVAILABLE => "Service Unavailable",
        _ => status.canonical_reason().unwrap_or("Error"),
    }
}

fn problem_code_for(status: StatusCode, has_validation_errors: bool) -> &'static str {
    if has_validation_errors {
        return "autumn.validation_failed";
    }

    match status {
        StatusCode::BAD_REQUEST => "autumn.bad_request",
        StatusCode::UNAUTHORIZED => "autumn.unauthorized",
        StatusCode::FORBIDDEN => "autumn.forbidden",
        StatusCode::NOT_FOUND => "autumn.not_found",
        StatusCode::GONE => "autumn.gone",
        StatusCode::CONFLICT => "autumn.conflict",
        StatusCode::PAYLOAD_TOO_LARGE => "autumn.payload_too_large",
        StatusCode::UNPROCESSABLE_ENTITY => "autumn.unprocessable_entity",
        StatusCode::INTERNAL_SERVER_ERROR => "autumn.internal_server_error",
        StatusCode::NOT_IMPLEMENTED => "autumn.not_implemented",
        StatusCode::SERVICE_UNAVAILABLE => "autumn.service_unavailable",
        _ if status.is_client_error() => "autumn.client_error",
        _ if status.is_server_error() => "autumn.server_error",
        _ => "autumn.error",
    }
}

fn server_error_detail(status: StatusCode) -> String {
    match status {
        StatusCode::SERVICE_UNAVAILABLE => "Service unavailable".to_owned(),
        StatusCode::NOT_IMPLEMENTED => "Not implemented".to_owned(),
        _ => "Internal server error".to_owned(),
    }
}

impl IntoResponse for AutumnError {
    fn into_response(self) -> Response {
        let message = self.inner.to_string();
        let (status, problem_type) = self.rendered_problem();

        let details = self.details.clone();
        let cache_idempotency_response = self.cache_idempotency_response;

        // Stash error metadata for exception filters to inspect without
        // parsing the response body.
        let error_info = crate::middleware::AutumnErrorInfo {
            status,
            message: message.clone(),
            details: details.clone(),
            problem_type,
            #[cfg(debug_assertions)]
            backtrace_string: self.backtrace_string.clone(),
            #[cfg(not(debug_assertions))]
            backtrace_string: None,
        };

        let body = problem_details(
            status,
            message,
            details.as_ref(),
            problem_type,
            None,
            None,
            true,
        );
        let mut response = (status, axum::Json(body)).into_response();
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/problem+json"),
        );
        if status == StatusCode::CONFLICT {
            response.headers_mut().insert(
                "HX-Trigger",
                HeaderValue::from_static(r#"{"autumn:conflict":true}"#),
            );
        }
        if cache_idempotency_response {
            response
                .extensions_mut()
                .insert(crate::idempotency::IdempotencyCacheCommittedErrorResponse);
        }
        response.extensions_mut().insert(error_info);
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    #[derive(Debug)]
    struct TestError(String);

    impl std::fmt::Display for TestError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.0)
        }
    }

    impl std::error::Error for TestError {}

    #[derive(Debug)]
    struct WrappedError {
        message: String,
        source: TestError,
    }

    impl std::fmt::Display for WrappedError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.message)
        }
    }

    impl std::error::Error for WrappedError {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.source)
        }
    }

    #[test]
    fn blanket_from_defaults_to_500() {
        let err: AutumnError = TestError("boom".into()).into();
        assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn internal_server_error_is_500() {
        let err = AutumnError::internal_server_error(TestError("boom".into()));
        assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn test_not_found_error() {
        let err = AutumnError::not_found(std::io::Error::other("no such user"));
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn not_found_is_404() {
        let err = AutumnError::not_found(TestError("missing".into()));
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn bad_request_is_400() {
        let err = AutumnError::bad_request(TestError("invalid input".into()));
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn unprocessable_is_422() {
        let err = AutumnError::unprocessable(TestError("bad entity".into()));
        assert_eq!(err.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn unauthorized_is_401() {
        let err = AutumnError::unauthorized(TestError("unauthorized".into()));
        assert_eq!(err.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn forbidden_is_403() {
        let err = AutumnError::forbidden(TestError("forbidden".into()));
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn validation_is_422() {
        let mut details = std::collections::HashMap::new();
        details.insert("field".to_string(), vec!["error".to_string()]);
        let err = AutumnError::validation(details);
        assert_eq!(err.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn service_unavailable_is_503() {
        let err = AutumnError::service_unavailable(TestError("pool exhausted".into()));
        assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn internal_server_error_msg_is_500() {
        let err = AutumnError::internal_server_error_msg("db failure");
        assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(err.to_string(), "db failure");
    }

    #[test]
    fn not_found_msg_is_404() {
        let err = AutumnError::not_found_msg("no such user");
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
        assert_eq!(err.to_string(), "no such user");
    }

    #[test]
    fn bad_request_msg_is_400() {
        let err = AutumnError::bad_request_msg("invalid input");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn unprocessable_msg_is_422() {
        let err = AutumnError::unprocessable_msg("title required");
        assert_eq!(err.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn unauthorized_msg_is_401() {
        let err = AutumnError::unauthorized_msg("login required");
        assert_eq!(err.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn forbidden_msg_is_403() {
        let err = AutumnError::forbidden_msg("no access");
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn service_unavailable_msg_is_503() {
        let err = AutumnError::service_unavailable_msg("db down");
        assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(err.to_string(), "db down");
    }

    #[test]
    fn with_status_overrides() {
        let err: AutumnError = TestError("forbidden".into()).into();
        let err = err.with_status(StatusCode::FORBIDDEN);
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn display_uses_inner_message() {
        let err: AutumnError = TestError("something broke".into()).into();
        assert_eq!(err.to_string(), "something broke");
    }

    #[test]
    fn source_chain_lists_inner_sources() {
        let err = AutumnError::internal_server_error(WrappedError {
            message: "failed to backfill".to_string(),
            source: TestError("database connection dropped".to_string()),
        });

        assert_eq!(
            err.source_chain(),
            vec!["database connection dropped".to_string()]
        );
    }

    #[test]
    fn into_response_has_correct_status() {
        let err = AutumnError::not_found(TestError("not found".into()));
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn into_response_has_json_body() -> Result<(), axum::Error> {
        let err = AutumnError::not_found(TestError("not found".into()));
        let response = err.into_response();

        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
        let json: serde_json::Value = serde_json::from_slice(&body).expect("valid json");

        assert_eq!(json["status"], 404);
        assert_eq!(json["detail"], "not found");
        assert_eq!(json["code"], "autumn.not_found");
        Ok(())
    }

    #[test]
    fn debug_shows_status_and_inner() {
        let err = AutumnError::bad_request(TestError("oops".into()));
        let debug = format!("{err:?}");
        assert!(debug.contains("AutumnError"));
        assert!(debug.contains("400"));
    }

    #[tokio::test]
    async fn msg_constructor_produces_valid_json_response() -> Result<(), axum::Error> {
        let err = AutumnError::unprocessable_msg("title required");
        let response = err.into_response();

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
        let json: serde_json::Value = serde_json::from_slice(&body).expect("valid json");
        assert_eq!(json["status"], 422);
        assert_eq!(json["detail"], "title required");
        assert_eq!(json["code"], "autumn.unprocessable_entity");
        Ok(())
    }

    #[tokio::test]
    async fn service_unavailable_response_is_503() -> Result<(), axum::Error> {
        let err = AutumnError::service_unavailable_msg("db down");
        let response = err.into_response();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
        let json: serde_json::Value = serde_json::from_slice(&body).expect("valid json");
        assert_eq!(json["status"], 503);
        assert_eq!(json["detail"], "db down");
        assert_eq!(json["code"], "autumn.service_unavailable");
        Ok(())
    }

    #[test]
    fn conflict_is_409() {
        let err = AutumnError::conflict(TestError("stale version".into()));
        assert_eq!(err.status(), StatusCode::CONFLICT);
    }

    #[test]
    fn conflict_msg_is_409() {
        let err = AutumnError::conflict_msg("please reload and retry");
        assert_eq!(err.status(), StatusCode::CONFLICT);
        assert_eq!(err.to_string(), "please reload and retry");
    }

    #[test]
    fn gone_is_410() {
        let err = AutumnError::gone(TestError("sunsetted".into()));
        assert_eq!(err.status(), StatusCode::GONE);
    }

    #[test]
    fn gone_msg_is_410() {
        let err = AutumnError::gone_msg("API version has been sunsetted");
        assert_eq!(err.status(), StatusCode::GONE);
        assert_eq!(err.to_string(), "API version has been sunsetted");
    }

    #[tokio::test]
    async fn conflict_response_is_409_json() -> Result<(), axum::Error> {
        let err = AutumnError::conflict_msg("version mismatch");
        let response = err.into_response();

        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
        let json: serde_json::Value = serde_json::from_slice(&body).expect("valid json");
        assert_eq!(json["status"], 409);
        assert_eq!(json["detail"], "version mismatch");
        assert_eq!(json["type"], "https://autumn.dev/problems/conflict");
        assert_eq!(json["title"], "Conflict");
        Ok(())
    }

    #[tokio::test]
    async fn conflict_response_has_hx_trigger_header() -> Result<(), axum::Error> {
        let err = AutumnError::conflict_msg("version mismatch");
        let response = err.into_response();

        assert_eq!(response.status(), StatusCode::CONFLICT);
        let hx_trigger = response
            .headers()
            .get("HX-Trigger")
            .expect("HX-Trigger header present");
        assert_eq!(hx_trigger, r#"{"autumn:conflict":true}"#);
        Ok(())
    }

    // ── #2423: Postgres rejects NUL in TEXT — that is client input ──────────

    #[cfg(feature = "db")]
    mod nul_byte_violation_tests {
        use super::*;
        use crate::error::is_nul_byte_violation;

        /// A `DatabaseErrorInformation` carrying only a server message —
        /// SQLSTATE is not exposed by diesel's trait (see
        /// `diesel-async`'s `pg/error_helper.rs`, which maps `22021` to
        /// `DatabaseErrorKind::Unknown` and drops the code), so the message is
        /// all the classifier has to go on.
        #[derive(Debug)]
        struct FakeMessageInfo(&'static str);

        impl diesel::result::DatabaseErrorInformation for FakeMessageInfo {
            fn message(&self) -> &str {
                self.0
            }
            fn details(&self) -> Option<&str> {
                None
            }
            fn hint(&self) -> Option<&str> {
                None
            }
            fn table_name(&self) -> Option<&str> {
                None
            }
            fn column_name(&self) -> Option<&str> {
                None
            }
            fn constraint_name(&self) -> Option<&str> {
                None
            }
            fn statement_position(&self) -> Option<i32> {
                None
            }
        }

        fn unknown_db_error(message: &'static str) -> diesel::result::Error {
            diesel::result::Error::DatabaseError(
                diesel::result::DatabaseErrorKind::Unknown,
                Box::new(FakeMessageInfo(message)),
            )
        }

        /// The exact message Postgres 16 returns for SQLSTATE `22021`.
        const PG_NUL_MESSAGE: &str = r#"invalid byte sequence for encoding "UTF8": 0x00"#;

        #[test]
        fn pg_nul_byte_error_maps_to_422_not_500() {
            let err: AutumnError = unknown_db_error(PG_NUL_MESSAGE).into();
            assert_eq!(err.status(), StatusCode::UNPROCESSABLE_ENTITY);
        }

        #[test]
        fn predicate_recognizes_the_pg_nul_byte_error() {
            let err: AutumnError = unknown_db_error(PG_NUL_MESSAGE).into();
            assert!(is_nul_byte_violation(&err));
        }

        /// `lc_messages` translates the prose but leaves the encoding name and
        /// the byte literal alone, so a non-English server still classifies.
        #[test]
        fn localized_pg_nul_byte_message_is_still_recognized() {
            let err: AutumnError =
                unknown_db_error("ungueltige Byte-Sequenz fuer Kodierung \u{ab}UTF8\u{bb}: 0x00")
                    .into();
            assert_eq!(err.status(), StatusCode::UNPROCESSABLE_ENTITY);
        }

        /// The raw server message must not reach the client: the 422 error page
        /// renders `message` verbatim where the 500 page redacts it, so
        /// downgrading the status without re-wrapping would newly expose a
        /// database internal on a user-facing page.
        #[test]
        fn the_raw_postgres_message_is_not_the_client_facing_message() {
            let err: AutumnError = unknown_db_error(PG_NUL_MESSAGE).into();
            assert_eq!(err.to_string(), crate::error::NUL_BYTE_REJECTED_MESSAGE);
            assert!(!err.to_string().contains("UTF8"));
        }

        /// ...but the original is still reachable for logs, error reporting and
        /// handler-side classification.
        #[test]
        fn the_original_diesel_error_survives_as_a_source() {
            let err: AutumnError = unknown_db_error(PG_NUL_MESSAGE).into();
            let inner = err
                .downcast_chain_ref::<diesel::result::Error>()
                .expect("the diesel error must stay reachable through the chain");
            assert!(inner.to_string().contains("0x00"));
        }

        /// Narrow by construction: any other database error keeps its 500, so
        /// a genuine server bug is never relabelled as the client's fault.
        #[test]
        fn other_db_errors_still_map_to_500() {
            for message in [
                "division by zero",
                "relation \"posts\" does not exist",
                // Mentions a byte literal but is not the encoding rejection.
                "invalid input syntax for type bytea: 0x00",
            ] {
                let err: AutumnError = unknown_db_error(message).into();
                assert_eq!(
                    err.status(),
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "unexpectedly reclassified: {message}"
                );
                assert!(!is_nul_byte_violation(&err), "false positive: {message}");
            }
        }

        /// Postgres echoes submitted text into the primary message of other
        /// errors. A client must not be able to spell the classifier's pattern
        /// inside its own input and so relabel a real server fault as its own
        /// fault — which would hide it from 5xx alerting and put the message on
        /// the 422 page. The end-anchor is what defeats this.
        #[test]
        fn attacker_supplied_text_echoed_into_a_message_does_not_classify() {
            for message in [
                // The value the client submitted, echoed and quoted.
                "invalid input syntax for type integer: \"UTF: 0x00\"",
                "invalid input syntax for type uuid: \"0x00 UTF8\"",
                // A dynamic identifier built from client input.
                "column \"0x00 UTF8 encoding\" does not exist",
            ] {
                let err: AutumnError = unknown_db_error(message).into();
                assert_eq!(
                    err.status(),
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "client-spellable message was reclassified: {message}"
                );
            }
        }

        /// The chain walk continues past a non-matching diesel error rather
        /// than stopping at the first one, and the predicate agrees with the
        /// status the `From` impl assigned.
        #[test]
        fn predicate_and_status_agree_through_a_wrapping_error() {
            #[derive(Debug)]
            struct Wrapper(diesel::result::Error);
            impl std::fmt::Display for Wrapper {
                fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    write!(f, "repository failed")
                }
            }
            impl std::error::Error for Wrapper {
                fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                    Some(&self.0)
                }
            }

            let err: AutumnError = Wrapper(unknown_db_error(PG_NUL_MESSAGE)).into();
            assert_eq!(err.status(), StatusCode::UNPROCESSABLE_ENTITY);
            assert!(
                is_nul_byte_violation(&err),
                "the predicate must agree with the status the From impl chose"
            );
        }

        #[test]
        fn predicate_is_false_for_non_db_errors() {
            let err = AutumnError::internal_server_error_msg(PG_NUL_MESSAGE);
            assert!(!is_nul_byte_violation(&err));
        }
    }

    // ── unique_violation_field (issue #1032) ────────────────────────────────

    #[cfg(feature = "db")]
    mod unique_violation_field_tests {
        use super::*;
        use crate::error::unique_violation_field;

        #[derive(Debug)]
        struct FakeDbErrorInfo {
            constraint: Option<&'static str>,
        }

        impl diesel::result::DatabaseErrorInformation for FakeDbErrorInfo {
            fn message(&self) -> &'static str {
                "duplicate key value violates unique constraint"
            }
            fn details(&self) -> Option<&str> {
                None
            }
            fn hint(&self) -> Option<&str> {
                None
            }
            fn table_name(&self) -> Option<&str> {
                None
            }
            fn column_name(&self) -> Option<&str> {
                None
            }
            fn constraint_name(&self) -> Option<&str> {
                self.constraint
            }
            fn statement_position(&self) -> Option<i32> {
                None
            }
        }

        fn unique_violation(constraint: Option<&'static str>) -> diesel::result::Error {
            diesel::result::Error::DatabaseError(
                diesel::result::DatabaseErrorKind::UniqueViolation,
                Box::new(FakeDbErrorInfo { constraint }),
            )
        }

        const MAPPING: &[(&str, &str, &str)] =
            &[("idx_users_email_unique", "email", "has already been taken")];

        #[test]
        fn matches_constraint_name_to_field_and_message() {
            let err: AutumnError = AutumnError::internal_server_error(unique_violation(Some(
                "idx_users_email_unique",
            )));
            assert_eq!(
                unique_violation_field(&err, MAPPING),
                Some(("email", "has already been taken"))
            );
        }

        #[test]
        fn returns_none_for_unrecognized_constraint() {
            let err: AutumnError =
                AutumnError::internal_server_error(unique_violation(Some("some_other_constraint")));
            assert_eq!(unique_violation_field(&err, MAPPING), None);
        }

        #[test]
        fn returns_none_when_constraint_name_is_absent() {
            let err: AutumnError = AutumnError::internal_server_error(unique_violation(None));
            assert_eq!(unique_violation_field(&err, MAPPING), None);
        }

        #[test]
        fn returns_none_for_non_unique_violation_db_errors() {
            let err: AutumnError =
                AutumnError::internal_server_error(diesel::result::Error::NotFound);
            assert_eq!(unique_violation_field(&err, MAPPING), None);
        }

        #[test]
        fn returns_none_for_non_db_errors() {
            let err = AutumnError::internal_server_error_msg("plain string error");
            assert_eq!(unique_violation_field(&err, MAPPING), None);
        }

        #[test]
        fn recovers_diesel_error_through_repository_style_blanket_from() {
            // Mirrors a generated repository method's `.execute(...).await?`
            // -- the diesel error is converted to `AutumnError` via the
            // blanket `From` impl before `unique_violation_field` ever sees
            // it, same as `repo.save(...).await?`'s already-mapped error.
            fn insert() -> Result<(), diesel::result::Error> {
                Err(unique_violation(Some("idx_users_email_unique")))
            }
            fn handler() -> Result<(), AutumnError> {
                insert()?;
                Ok(())
            }
            let err = handler().unwrap_err();
            assert_eq!(
                unique_violation_field(&err, MAPPING),
                Some(("email", "has already been taken"))
            );
        }
    }

    // ── #2587: read accessors for validation details and problem code ──

    fn details_map(pairs: &[(&str, &[&str])]) -> std::collections::HashMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(field, messages)| {
                (
                    (*field).to_owned(),
                    messages.iter().map(|m| (*m).to_owned()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn details_reads_back_the_validation_map() {
        let err = AutumnError::validation(details_map(&[("title", &["must be 1-120 characters"])]));
        let details = err.details().expect("validation error carries details");
        assert_eq!(
            details.get("title").map(Vec::as_slice),
            Some(["must be 1-120 characters".to_owned()].as_slice())
        );
    }

    #[test]
    fn details_is_none_for_non_validation_errors() {
        assert!(
            AutumnError::not_found_msg("no such user")
                .details()
                .is_none()
        );
        assert!(
            AutumnError::conflict_msg("stale version")
                .details()
                .is_none()
        );
    }

    #[test]
    fn code_names_each_constructor() {
        let cases: Vec<(AutumnError, &str)> = vec![
            (
                AutumnError::validation(details_map(&[("title", &["too short"])])),
                "autumn.validation_failed",
            ),
            (AutumnError::not_found_msg("gone"), "autumn.not_found"),
            (AutumnError::bad_request_msg("bad"), "autumn.bad_request"),
            (AutumnError::conflict_msg("stale"), "autumn.conflict"),
            (AutumnError::gone_msg("sunset"), "autumn.gone"),
            (
                AutumnError::query_timeout("statement_timeout"),
                "autumn.query_timeout",
            ),
            (
                AutumnError::internal_server_error_msg("boom"),
                "autumn.internal_server_error",
            ),
        ];

        for (err, expected) in cases {
            assert_eq!(err.code(), expected, "code() for {err:?}");
        }
    }

    #[test]
    fn code_follows_the_statement_timeout_reclassification() {
        // `into_response` demotes a 500 whose message names a cancelled
        // statement to a 503 `autumn.query_timeout`; `code()` says the same.
        let err =
            AutumnError::internal_server_error_msg("canceling statement due to statement timeout");
        assert_eq!(err.code(), "autumn.query_timeout");
    }

    #[test]
    fn display_lists_the_failing_fields() {
        let err = AutumnError::validation(details_map(&[
            ("title", &["must be 1-120 characters"]),
            ("email", &["invalid"]),
        ]));
        assert_eq!(
            err.to_string(),
            "Validation failed: email: invalid; title: must be 1-120 characters"
        );
    }

    #[test]
    fn display_field_order_does_not_follow_the_map() {
        // A two-field map lands sorted by chance about four runs in ten, so a
        // dropped sort survives that test. Five fields, rebuilt each round,
        // fail every run instead.
        let fields: &[(&str, &[&str])] = &[
            ("title", &["e"]),
            ("email", &["e"]),
            ("author", &["e"]),
            ("body", &["e"]),
            ("slug", &["e"]),
        ];
        let expected = "Validation failed: author: e; body: e; email: e; slug: e; title: e";

        for _ in 0..64 {
            assert_eq!(
                AutumnError::validation(details_map(fields)).to_string(),
                expected
            );
        }
    }

    #[test]
    fn display_sorts_non_ascii_fields_deterministically() {
        let fields: &[(&str, &[&str])] = &[
            ("überschrift", &["zu kurz"]),
            ("邮箱", &["无效"]),
            ("email", &["invalide"]),
        ];
        let first = AutumnError::validation(details_map(fields)).to_string();
        for _ in 0..32 {
            assert_eq!(
                AutumnError::validation(details_map(fields)).to_string(),
                first
            );
        }
        assert!(first.contains("邮箱: 无效"), "{first}");
    }

    #[test]
    fn message_is_not_redacted_for_a_server_error() {
        // The doc caveat, pinned: outside a dev profile the response replaces
        // a 5xx `detail`, but `message` still returns the wrapped error.
        let err = AutumnError::internal_server_error_msg("password=hunter2 in dsn");
        assert_eq!(err.message(), "password=hunter2 in dsn");

        let redacted = problem_details(err.status(), err.message(), None, None, None, None, false);
        assert_eq!(redacted.detail, "Internal server error");
    }

    #[test]
    fn message_is_the_wrapped_error_without_the_fields() {
        let err = AutumnError::validation(details_map(&[("email", &["invalid"])]));
        assert_eq!(err.message(), "Validation failed");
        assert_eq!(err.to_string(), "Validation failed: email: invalid");
        assert_eq!(AutumnError::not_found_msg("gone").message(), "gone");
    }

    #[test]
    fn display_joins_multiple_messages_for_one_field() {
        let err = AutumnError::validation(details_map(&[(
            "password",
            &["too short", "needs a digit"],
        )]));
        assert_eq!(
            err.to_string(),
            "Validation failed: password: too short, needs a digit"
        );
    }

    #[test]
    fn display_falls_back_to_the_title_without_field_messages() {
        assert_eq!(
            AutumnError::validation(details_map(&[])).to_string(),
            "Validation failed"
        );
        assert_eq!(
            AutumnError::validation(details_map(&[("title", &[])])).to_string(),
            "Validation failed"
        );
    }

    #[test]
    fn display_is_unchanged_for_non_validation_errors() {
        assert_eq!(
            AutumnError::not_found_msg("no such user").to_string(),
            "no such user"
        );
    }

    #[tokio::test]
    async fn code_equals_the_code_in_the_rendered_body() -> Result<(), axum::Error> {
        let builders: Vec<fn() -> AutumnError> = vec![
            || AutumnError::validation(details_map(&[("title", &["too short"])])),
            || AutumnError::not_found_msg("gone"),
            || AutumnError::conflict_msg("stale"),
            || AutumnError::gone_msg("sunset"),
            || AutumnError::query_timeout("statement_timeout"),
            || AutumnError::internal_server_error_msg("boom"),
            || AutumnError::internal_server_error_msg("ERROR: 57014 query canceled"),
        ];

        for build in builders {
            let expected = build().code();
            let body =
                axum::body::to_bytes(build().into_response().into_body(), usize::MAX).await?;
            let json: serde_json::Value = serde_json::from_slice(&body).expect("valid json");
            assert_eq!(json["code"], &*expected);
        }
        Ok(())
    }
}
