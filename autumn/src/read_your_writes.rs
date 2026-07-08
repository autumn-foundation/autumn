//! Read-your-own-writes (RYWW) routing support.
//!
//! When `database.read_your_writes` is `request` or `session`, Autumn installs
//! a per-request task-local that generated repository read methods consult at
//! acquire time. Once the current request has checked out a **primary** connection
//! (via the `Db` extractor or a generated mutating method), subsequent
//! replica-eligible reads are redirected to the primary pool — preventing the
//! classic stale-read anomaly that arises when replication lag is non-zero.
//!
//! When `read_your_writes` is `off` (the default), **none of this module's
//! code is reachable from hot paths** — `is_pinned()` fast-returns `false`
//! without touching the task-local, and no middleware layer is installed.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::config::ReadYourWrites;

struct Inner {
    mode: ReadYourWrites,
    incoming_pin: bool,
    wrote: AtomicBool,
    /// Set on the first pin-redirect trace so subsequent redirects within the
    /// same request don't produce unbounded log volume.
    pin_traced: AtomicBool,
    metrics: Option<crate::middleware::MetricsCollector>,
}

/// Per-request pin state, cheaply cloneable via `Arc`.
#[derive(Clone)]
pub struct RequestPin {
    inner: Arc<Inner>,
}

impl RequestPin {
    /// Build a basic pin for `request` mode (or `session` without a cookie).
    #[must_use]
    pub fn new(mode: ReadYourWrites) -> Self {
        Self {
            inner: Arc::new(Inner {
                mode,
                incoming_pin: false,
                wrote: AtomicBool::new(false),
                pin_traced: AtomicBool::new(false),
                metrics: None,
            }),
        }
    }

    /// Build a pin that also records metrics when a redirect occurs.
    #[must_use]
    pub fn new_with_metrics(
        mode: ReadYourWrites,
        metrics: crate::middleware::MetricsCollector,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                mode,
                incoming_pin: false,
                wrote: AtomicBool::new(false),
                pin_traced: AtomicBool::new(false),
                metrics: Some(metrics),
            }),
        }
    }

    /// Build a session-mode pin, parsing a signed cookie value.
    ///
    /// Cookie format: `{unix_timestamp_secs}.{hmac_hex}`.
    /// `incoming_pin` is set when the signature is valid and the timestamp
    /// is within `window_secs` of now.
    #[must_use]
    pub fn with_session_cookie(
        cookie: &str,
        keys: &crate::security::config::ResolvedSigningKeys,
        window_secs: u64,
    ) -> Self {
        let incoming_pin = parse_session_cookie(cookie, keys, window_secs);
        Self {
            inner: Arc::new(Inner {
                mode: ReadYourWrites::Session,
                incoming_pin,
                wrote: AtomicBool::new(false),
                pin_traced: AtomicBool::new(false),
                metrics: None,
            }),
        }
    }

    /// Build a pin with an explicit `incoming_pin` flag, bypassing cookie
    /// parsing. Intended for integration tests that need to verify session-mode
    /// routing behavior without constructing a real signed cookie.
    #[doc(hidden)]
    #[must_use]
    pub fn with_incoming_pin(mode: ReadYourWrites, incoming_pin: bool) -> Self {
        Self {
            inner: Arc::new(Inner {
                mode,
                incoming_pin,
                wrote: AtomicBool::new(false),
                pin_traced: AtomicBool::new(false),
                metrics: None,
            }),
        }
    }

    /// Build a session-mode pin with metrics, parsing a signed cookie.
    #[must_use]
    pub fn with_session_cookie_and_metrics(
        cookie: &str,
        keys: &crate::security::config::ResolvedSigningKeys,
        window_secs: u64,
        metrics: crate::middleware::MetricsCollector,
    ) -> Self {
        let incoming_pin = parse_session_cookie(cookie, keys, window_secs);
        Self {
            inner: Arc::new(Inner {
                mode: ReadYourWrites::Session,
                incoming_pin,
                wrote: AtomicBool::new(false),
                pin_traced: AtomicBool::new(false),
                metrics: Some(metrics),
            }),
        }
    }

    /// Returns `true` when the cross-request session cookie was valid and fresh.
    #[must_use]
    pub fn incoming_pin(&self) -> bool {
        self.inner.incoming_pin
    }

    /// Returns `true` when a write has been marked in this request scope.
    #[must_use]
    pub fn wrote(&self) -> bool {
        self.inner.wrote.load(Ordering::Relaxed)
    }
}

/// Parse and validate the `autumn.ryw` signed cookie.
///
/// Returns `true` only when the HMAC signature is valid and the timestamp
/// is within `window_secs` of the current wall time.
fn parse_session_cookie(
    cookie: &str,
    keys: &crate::security::config::ResolvedSigningKeys,
    window_secs: u64,
) -> bool {
    let Some((ts_str, sig)) = cookie.rsplit_once('.') else {
        return false;
    };
    let Ok(ts) = ts_str.parse::<u64>() else {
        return false;
    };
    if !keys.verify(ts_str.as_bytes(), sig) {
        return false;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Reject timestamps more than 5 s in the future so clock skew on a signing
    // server can't produce a cookie that is accepted indefinitely on other nodes.
    if ts > now {
        ts - now < 5
    } else {
        now - ts < window_secs
    }
}

/// Build the value for a `Set-Cookie: autumn.ryw=…` response header.
///
/// Returns `None` when the pin mode is not `Session` or no write occurred
/// in this scope — callers should only set the cookie when this returns `Some`.
///
/// The cookie value is `{unix_secs}.{hmac_hex}`, matching the format parsed
/// by [`RequestPin::with_session_cookie`].
#[must_use]
pub fn session_cookie_value(
    pin: &RequestPin,
    keys: &crate::security::config::ResolvedSigningKeys,
) -> Option<String> {
    if !matches!(pin.inner.mode, ReadYourWrites::Session) {
        return None;
    }
    if !pin.inner.wrote.load(Ordering::Relaxed) {
        return None;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let ts_str = now.to_string();
    let sig = keys.sign(ts_str.as_bytes());
    Some(format!("{ts_str}.{sig}"))
}

tokio::task_local! {
    static PIN: RequestPin;
}

/// Install the task-local pin for a request and run `fut` within its scope.
///
/// Called by the RYW middleware for every request when mode is not `off`.
pub async fn scope<F: std::future::Future>(pin: RequestPin, fut: F) -> F::Output {
    PIN.scope(pin, fut).await
}

/// Mark that the current request has performed a primary write.
///
/// No-op when called outside a [`scope`] (i.e. when `read_your_writes = "off"`
/// and no middleware installed the task-local). Safe to call unconditionally
/// from `Db::from_request_parts` and generated mutating methods.
pub fn mark_write() {
    PIN.try_with(|pin| {
        pin.inner.wrote.store(true, Ordering::Relaxed);
    })
    .ok();
}

/// Returns `true` when the task-local pin is active and reads should be
/// redirected to the primary.
///
/// Fast path: if the task-local is absent (no scope installed, i.e. `off`
/// mode), returns `false` in O(1) with no heap allocation.
#[inline]
#[must_use]
pub fn is_pinned() -> bool {
    PIN.try_with(|pin| {
        matches!(
            pin.inner.mode,
            ReadYourWrites::Request | ReadYourWrites::Session
        ) && (pin.inner.wrote.load(Ordering::Relaxed) || pin.inner.incoming_pin)
    })
    .unwrap_or(false)
}

/// Record a pin-redirected read: increment the metric and emit a trace event.
///
/// Called by generated repository read methods when a replica-eligible read is
/// redirected to the primary. The `try_with` is defensive — the task-local is
/// expected to be set when this is called.
pub fn note_pin_redirect() {
    PIN.try_with(|pin| {
        if let Some(ref metrics) = pin.inner.metrics {
            metrics.record_read_your_writes_pin();
        }
        // Emit the trace at most once per request to avoid log spam on
        // read-heavy handlers where every read is redirected.
        if !pin.inner.pin_traced.swap(true, Ordering::Relaxed) {
            tracing::debug!(
                target: "autumn::db",
                ryw_pinned = true,
                "read redirected to primary (read-your-own-writes pin active)"
            );
        }
    })
    .ok();
}

/// Name of the signed cross-request session cookie.
pub const RYW_COOKIE_NAME: &str = "autumn.ryw";

/// Axum middleware function that installs the RYWW task-local for every
/// request and, in `session` mode, handles the signed cookie lifecycle.
///
/// Installed by `apply_middleware` in `router.rs` when
/// `database.read_your_writes != "off"`.
pub async fn middleware(
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
    mode: crate::config::ReadYourWrites,
    window_secs: u64,
    keys: Option<std::sync::Arc<crate::security::config::ResolvedSigningKeys>>,
    metrics: crate::middleware::MetricsCollector,
) -> axum::http::Response<axum::body::Body> {
    let pin = match mode {
        crate::config::ReadYourWrites::Session => {
            let cookie_val = extract_ryw_cookie_value(&req);
            match (cookie_val, &keys) {
                (Some(cv), Some(k)) => {
                    RequestPin::with_session_cookie_and_metrics(&cv, k, window_secs, metrics)
                }
                _ => RequestPin::new_with_metrics(mode, metrics),
            }
        }
        crate::config::ReadYourWrites::Request => RequestPin::new_with_metrics(mode, metrics),
        crate::config::ReadYourWrites::Off => unreachable!("RYW middleware installed in off mode"),
    };

    let pin_for_response = pin.clone();

    let mut response = scope(pin, next.run(req)).await;

    // Session mode: stamp a Set-Cookie if a write occurred so subsequent
    // requests within the freshness window also route to primary.
    if mode == crate::config::ReadYourWrites::Session
        && let Some(k) = &keys
        && let Some(cv) = session_cookie_value(&pin_for_response, k)
    {
        let cookie_str = format!(
            "{RYW_COOKIE_NAME}={cv}; Max-Age={window_secs}; HttpOnly; \
             Secure; SameSite=Lax; Path=/"
        );
        if let Ok(hv) = axum::http::HeaderValue::from_str(&cookie_str) {
            response
                .headers_mut()
                .append(axum::http::header::SET_COOKIE, hv);
        }
    }

    response
}

/// Extract the `autumn.ryw` cookie value from raw `Cookie` headers.
///
/// Delegates to `session::get_cookie` so that duplicate-name rejection
/// (cookie-tossing mitigation) and exact-name matching are handled uniformly
/// with the session layer.
fn extract_ryw_cookie_value(req: &axum::http::Request<axum::body::Body>) -> Option<String> {
    crate::session::get_cookie(req.headers(), RYW_COOKIE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mark_write_no_op_outside_scope() {
        mark_write(); // must not panic
        assert!(!is_pinned());
    }

    #[tokio::test]
    async fn is_pinned_false_outside_scope() {
        assert!(!is_pinned());
    }

    #[tokio::test]
    async fn is_pinned_request_mode_before_write() {
        let pin = RequestPin::new(ReadYourWrites::Request);
        scope(pin, async {
            assert!(!is_pinned(), "no write yet");
        })
        .await;
    }

    #[tokio::test]
    async fn is_pinned_request_mode_after_write() {
        let pin = RequestPin::new(ReadYourWrites::Request);
        scope(pin, async {
            mark_write();
            assert!(is_pinned(), "write marked");
        })
        .await;
    }

    #[tokio::test]
    async fn is_pinned_off_mode_never_pins() {
        let pin = RequestPin::new(ReadYourWrites::Off);
        scope(pin, async {
            mark_write();
            assert!(!is_pinned(), "off mode must never pin");
        })
        .await;
    }

    #[tokio::test]
    async fn incoming_pin_pins_without_write() {
        let pin = RequestPin::with_incoming_pin(ReadYourWrites::Session, true);
        scope(pin, async {
            assert!(is_pinned(), "incoming_pin should activate the pin");
        })
        .await;
    }

    // Cookie parsing tests (use pub(crate) ResolvedSigningKeys directly)
    fn test_keys() -> crate::security::config::ResolvedSigningKeys {
        crate::security::config::ResolvedSigningKeys::new(b"test-key-for-ryw-unit".to_vec(), vec![])
    }

    fn fresh_cookie(keys: &crate::security::config::ResolvedSigningKeys) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let ts = now.to_string();
        let sig = keys.sign(ts.as_bytes());
        format!("{ts}.{sig}")
    }

    #[test]
    fn fresh_cookie_sets_incoming_pin() {
        let keys = test_keys();
        let cookie = fresh_cookie(&keys);
        let pin = RequestPin::with_session_cookie(&cookie, &keys, 5);
        assert!(
            pin.incoming_pin(),
            "fresh signed cookie must set incoming_pin"
        );
    }

    #[test]
    fn expired_cookie_does_not_set_incoming_pin() {
        let keys = test_keys();
        let ts = 1_000u64.to_string(); // Jan 1970 — clearly expired
        let sig = keys.sign(ts.as_bytes());
        let cookie = format!("{ts}.{sig}");
        let pin = RequestPin::with_session_cookie(&cookie, &keys, 5);
        assert!(
            !pin.incoming_pin(),
            "expired cookie must NOT set incoming_pin"
        );
    }

    #[test]
    fn tampered_cookie_does_not_set_incoming_pin() {
        let keys = test_keys();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let cookie = format!("{now}.deadbeef");
        let pin = RequestPin::with_session_cookie(&cookie, &keys, 5);
        assert!(
            !pin.incoming_pin(),
            "cookie with invalid HMAC must NOT set incoming_pin"
        );
    }

    #[test]
    fn future_cookie_beyond_skew_tolerance_does_not_set_incoming_pin() {
        let keys = test_keys();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // 60 s in the future — well beyond the 5 s skew tolerance.
        let ts = (now + 60).to_string();
        let sig = keys.sign(ts.as_bytes());
        let cookie = format!("{ts}.{sig}");
        let pin = RequestPin::with_session_cookie(&cookie, &keys, 5);
        assert!(
            !pin.incoming_pin(),
            "far-future cookie must NOT set incoming_pin"
        );
    }

    #[test]
    fn malformed_cookie_does_not_set_incoming_pin() {
        let keys = test_keys();
        for bad in &["", "notimestamp", "abc.def.ghi"] {
            let pin = RequestPin::with_session_cookie(bad, &keys, 5);
            assert!(
                !pin.incoming_pin(),
                "malformed cookie {bad:?} must NOT set incoming_pin"
            );
        }
    }

    #[test]
    fn session_cookie_value_returns_none_for_request_mode() {
        let keys = test_keys();
        let pin = RequestPin::new(ReadYourWrites::Request);
        assert!(session_cookie_value(&pin, &keys).is_none());
    }

    #[test]
    fn session_cookie_value_returns_none_when_no_write() {
        let keys = test_keys();
        let pin = RequestPin::new(ReadYourWrites::Session);
        assert!(session_cookie_value(&pin, &keys).is_none());
    }

    #[test]
    fn session_cookie_value_returns_value_after_write() {
        let keys = test_keys();
        let pin = RequestPin::new(ReadYourWrites::Session);
        // Manually simulate mark_write on the pin.
        pin.inner.wrote.store(true, Ordering::Relaxed);
        let val = session_cookie_value(&pin, &keys);
        assert!(
            val.is_some(),
            "session mode + wrote must produce a cookie value"
        );
        let val = val.unwrap();
        // Must be parseable as a fresh cookie.
        let fresh_pin = RequestPin::with_session_cookie(&val, &keys, 5);
        assert!(
            fresh_pin.incoming_pin(),
            "produced cookie must be parseable as a fresh incoming_pin"
        );
    }

    #[tokio::test]
    async fn note_pin_redirect_increments_metric_via_new_with_metrics() {
        let metrics = crate::middleware::MetricsCollector::new();
        let pin = RequestPin::new_with_metrics(ReadYourWrites::Request, metrics.clone());
        scope(pin, async {
            mark_write();
            note_pin_redirect();
        })
        .await;
        assert_eq!(
            metrics.snapshot().read_your_writes_pins_total,
            1,
            "note_pin_redirect must increment the metric counter"
        );
    }

    #[tokio::test]
    async fn note_pin_redirect_trace_fires_only_once_per_request() {
        let metrics = crate::middleware::MetricsCollector::new();
        let pin = RequestPin::new_with_metrics(ReadYourWrites::Request, metrics.clone());
        scope(pin, async {
            mark_write();
            note_pin_redirect();
            note_pin_redirect();
            note_pin_redirect();
        })
        .await;
        // Metric increments on every call; trace deduplicated (can't assert trace
        // but we verify the counter reflects all three calls).
        assert_eq!(metrics.snapshot().read_your_writes_pins_total, 3);
    }

    #[test]
    fn with_session_cookie_and_metrics_fresh_sets_incoming_pin() {
        let keys = test_keys();
        let cookie = fresh_cookie(&keys);
        let metrics = crate::middleware::MetricsCollector::new();
        let pin = RequestPin::with_session_cookie_and_metrics(&cookie, &keys, 5, metrics);
        assert!(
            pin.incoming_pin(),
            "with_session_cookie_and_metrics: fresh cookie must set incoming_pin"
        );
    }

    #[test]
    fn with_session_cookie_and_metrics_expired_does_not_set_incoming_pin() {
        let keys = test_keys();
        let ts = 1_000u64.to_string();
        let sig = keys.sign(ts.as_bytes());
        let cookie = format!("{ts}.{sig}");
        let metrics = crate::middleware::MetricsCollector::new();
        let pin = RequestPin::with_session_cookie_and_metrics(&cookie, &keys, 5, metrics);
        assert!(
            !pin.incoming_pin(),
            "with_session_cookie_and_metrics: expired cookie must NOT set incoming_pin"
        );
    }

    // ── middleware() integration tests ────────────────────────────────────────

    #[tokio::test]
    async fn middleware_request_mode_no_set_cookie() {
        use axum::{Router, body::Body, http::Request, routing::get};
        use tower::ServiceExt;

        let metrics = crate::middleware::MetricsCollector::new();
        let app =
            Router::new()
                .route("/", get(|| async { "ok" }))
                .layer(axum::middleware::from_fn(move |req, next| {
                    middleware(req, next, ReadYourWrites::Request, 5, None, metrics.clone())
                }));

        let req = Request::builder().uri("/").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert!(
            !resp.headers().contains_key("set-cookie"),
            "request mode must not set a cookie"
        );
    }

    #[tokio::test]
    async fn middleware_session_mode_write_sets_cookie() {
        use axum::{Router, body::Body, http::Request, routing::get};
        use tower::ServiceExt;

        let keys = std::sync::Arc::new(test_keys());
        let metrics = crate::middleware::MetricsCollector::new();
        let keys_clone = keys.clone();
        let app = Router::new()
            .route(
                "/",
                get(|| async {
                    mark_write();
                    "ok"
                }),
            )
            .layer(axum::middleware::from_fn(move |req, next| {
                middleware(
                    req,
                    next,
                    ReadYourWrites::Session,
                    5,
                    Some(keys_clone.clone()),
                    metrics.clone(),
                )
            }));

        let req = Request::builder().uri("/").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let set_cookie = resp.headers().get("set-cookie");
        assert!(
            set_cookie.is_some(),
            "session mode + write must set the autumn.ryw cookie"
        );
        let cv = set_cookie.unwrap().to_str().unwrap();
        assert!(
            cv.starts_with("autumn.ryw="),
            "Set-Cookie must be the autumn.ryw cookie, got: {cv}"
        );
    }

    #[tokio::test]
    async fn middleware_session_mode_no_write_no_set_cookie() {
        use axum::{Router, body::Body, http::Request, routing::get};
        use tower::ServiceExt;

        let keys = std::sync::Arc::new(test_keys());
        let metrics = crate::middleware::MetricsCollector::new();
        let keys_clone = keys.clone();
        let app =
            Router::new()
                .route("/", get(|| async { "ok" }))
                .layer(axum::middleware::from_fn(move |req, next| {
                    middleware(
                        req,
                        next,
                        ReadYourWrites::Session,
                        5,
                        Some(keys_clone.clone()),
                        metrics.clone(),
                    )
                }));

        let req = Request::builder().uri("/").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert!(
            !resp.headers().contains_key("set-cookie"),
            "session mode without write must NOT set a cookie"
        );
    }

    #[tokio::test]
    async fn middleware_session_mode_incoming_cookie_pins_read() {
        use axum::{Router, body::Body, http::Request, routing::get};
        use tower::ServiceExt;

        let keys = std::sync::Arc::new(test_keys());
        let cookie = fresh_cookie(&keys);
        let metrics = crate::middleware::MetricsCollector::new();
        let keys_clone = keys.clone();

        // Track whether the handler saw a pin via a shared flag.
        let saw_pin = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let saw_pin_clone = saw_pin.clone();
        let app = Router::new()
            .route(
                "/",
                get(move || {
                    let flag = saw_pin_clone.clone();
                    async move {
                        flag.store(is_pinned(), Ordering::Relaxed);
                        "ok"
                    }
                }),
            )
            .layer(axum::middleware::from_fn(move |req, next| {
                middleware(
                    req,
                    next,
                    ReadYourWrites::Session,
                    5,
                    Some(keys_clone.clone()),
                    metrics.clone(),
                )
            }));

        let req = Request::builder()
            .uri("/")
            .header("cookie", format!("autumn.ryw={cookie}"))
            .body(Body::empty())
            .unwrap();
        app.oneshot(req).await.unwrap();
        assert!(
            saw_pin.load(Ordering::Relaxed),
            "a valid incoming autumn.ryw cookie must activate the pin inside the handler"
        );
    }
}
