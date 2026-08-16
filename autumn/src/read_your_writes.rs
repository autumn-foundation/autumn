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
    scope_future(pin, fut).await
}

/// [`scope`] as a **named** future rather than an `async fn`.
///
/// [`ReadYourWritesService`] needs the concrete type so its own `Service::Future`
/// can be named and therefore need no `Box::pin` (issue #2214); `scope` above is
/// the ergonomic `async fn` wrapper every other caller uses.
pub(crate) fn scope_future<F: std::future::Future>(
    pin: RequestPin,
    fut: F,
) -> tokio::task::futures::TaskLocalFuture<RequestPin, F> {
    PIN.scope(pin, fut)
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
/// The framework itself installs [`ReadYourWritesLayer`] (the same logic as a
/// `tower::Service` with a named future — see issue #2214) rather than this
/// function; it is retained as public API for callers wiring the middleware
/// into an `axum::middleware::from_fn` of their own.
pub async fn middleware(
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
    mode: crate::config::ReadYourWrites,
    window_secs: u64,
    keys: Option<std::sync::Arc<crate::security::config::ResolvedSigningKeys>>,
    metrics: crate::middleware::MetricsCollector,
) -> axum::http::Response<axum::body::Body> {
    let settings = ReadYourWritesSettings {
        mode,
        window_secs,
        keys,
        metrics,
    };
    let pin = request_pin_for(&req, &settings);
    let pin_for_response = pin.clone();

    let mut response = scope(pin, next.run(req)).await;

    // Session mode: stamp a Set-Cookie if a write occurred so subsequent
    // requests within the freshness window also route to primary.
    stamp_ryw_cookie(&mut response, &settings, &pin_for_response);

    response
}

/// Extract the `autumn.ryw` cookie value from raw `Cookie` headers.
///
/// Delegates to `session::get_cookie` so that duplicate-name rejection
/// (cookie-tossing mitigation) and exact-name matching are handled uniformly
/// with the session layer.
fn extract_ryw_cookie_value<B>(req: &axum::http::Request<B>) -> Option<String> {
    crate::session::get_cookie(req.headers(), RYW_COOKIE_NAME)
}

/// Everything the read-your-own-writes middleware resolves once at
/// router-assembly time.
///
/// Behind an `Arc` in the layer because the produced service is cloned on every
/// traversal of the ingress stack above it.
struct ReadYourWritesSettings {
    mode: crate::config::ReadYourWrites,
    window_secs: u64,
    keys: Option<Arc<crate::security::config::ResolvedSigningKeys>>,
    metrics: crate::middleware::MetricsCollector,
}

/// Tower [`Layer`](tower::Layer) form of [`middleware`], used by the framework's
/// ingress stack.
///
/// The `axum::middleware::from_fn` closure it replaces `Box::pin`ned its async
/// block on every request and cloned the erased service beneath it to move it in
/// (issue #2214). `PIN.scope(..)` already yields a named
/// [`tokio::task::futures::TaskLocalFuture`], so the pin can be installed with
/// no allocation at all.
#[derive(Clone)]
pub(crate) struct ReadYourWritesLayer {
    settings: Arc<ReadYourWritesSettings>,
}

impl ReadYourWritesLayer {
    /// Build the layer. `mode` must not be [`ReadYourWrites::Off`] — the
    /// framework only installs this layer when read-your-own-writes is on.
    pub(crate) fn new(
        mode: crate::config::ReadYourWrites,
        window_secs: u64,
        keys: Option<Arc<crate::security::config::ResolvedSigningKeys>>,
        metrics: crate::middleware::MetricsCollector,
    ) -> Self {
        Self {
            settings: Arc::new(ReadYourWritesSettings {
                mode,
                window_secs,
                keys,
                metrics,
            }),
        }
    }
}

impl<S> tower::Layer<S> for ReadYourWritesLayer {
    type Service = ReadYourWritesService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ReadYourWritesService {
            inner,
            settings: Arc::clone(&self.settings),
        }
    }
}

/// Tower [`Service`](tower::Service) produced by [`ReadYourWritesLayer`].
#[derive(Clone)]
pub(crate) struct ReadYourWritesService<S> {
    inner: S,
    settings: Arc<ReadYourWritesSettings>,
}

impl<S, ReqBody> tower::Service<axum::http::Request<ReqBody>> for ReadYourWritesService<S>
where
    S: tower::Service<axum::http::Request<ReqBody>, Response = axum::response::Response>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = ReadYourWritesFuture<S::Future>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: axum::http::Request<ReqBody>) -> Self::Future {
        let pin = request_pin_for(&req, &self.settings);
        let pin_for_response = pin.clone();
        // Built inside the scope, not merely polled inside it: arguments are
        // evaluated first, so passing `self.inner.call(req)` straight to
        // `scope_future` would run the synchronous `Service::call` chain beneath
        // this layer with `PIN` unset. `is_pinned()`/`mark_write()` are public
        // and synchronous, so an operator's own Tower layer is entitled to call
        // them from `call`. See `crate::capsule::capture` for the same hazard.
        let inner = PIN.sync_scope(pin.clone(), || self.inner.call(req));
        ReadYourWritesFuture {
            inner: scope_future(pin, inner),
            settings: Arc::clone(&self.settings),
            pin: pin_for_response,
        }
    }
}

/// Mint the per-request pin, reading the signed `autumn.ryw` cookie in session
/// mode. Shared by [`middleware`] and [`ReadYourWritesService`].
fn request_pin_for<B>(
    req: &axum::http::Request<B>,
    settings: &ReadYourWritesSettings,
) -> RequestPin {
    match settings.mode {
        crate::config::ReadYourWrites::Session => {
            match (extract_ryw_cookie_value(req), &settings.keys) {
                (Some(cv), Some(k)) => RequestPin::with_session_cookie_and_metrics(
                    &cv,
                    k,
                    settings.window_secs,
                    settings.metrics.clone(),
                ),
                _ => RequestPin::new_with_metrics(settings.mode, settings.metrics.clone()),
            }
        }
        crate::config::ReadYourWrites::Request => {
            RequestPin::new_with_metrics(settings.mode, settings.metrics.clone())
        }
        crate::config::ReadYourWrites::Off => {
            // Unreachable: both call sites gate on `mode != Off`
            // (`apply_middleware` only builds the layer inside that check, and
            // `middleware` documents the same precondition). This used to be an
            // `unreachable!()`; it is a `debug_assert!` plus an INERT pin now,
            // because turning a caller's misconfiguration into a panic on the
            // request path is the wrong trade for a middleware whose whole job
            // is optional. The fallback is `Off`, not `Request`: `Request` is
            // the *active* mode, so it would silently start pinning reads to
            // the primary for an app that configured read-your-own-writes off —
            // contradicting `is_pinned_off_mode_never_pins` below. `Off` is
            // inert (`is_pinned` is false, no cookie is minted), which is what
            // the configuration asked for.
            debug_assert!(
                false,
                "read-your-own-writes middleware built in `off` mode; \
                 both call sites are supposed to gate on `mode != Off`"
            );
            RequestPin::new_with_metrics(
                crate::config::ReadYourWrites::Off,
                settings.metrics.clone(),
            )
        }
    }
}

/// Stamp the `autumn.ryw` `Set-Cookie` on `response` when session mode recorded
/// a primary write. Shared by [`middleware`] and [`ReadYourWritesFuture`].
fn stamp_ryw_cookie(
    response: &mut axum::response::Response,
    settings: &ReadYourWritesSettings,
    pin: &RequestPin,
) {
    if settings.mode == crate::config::ReadYourWrites::Session
        && let Some(k) = &settings.keys
        && let Some(cv) = session_cookie_value(pin, k)
    {
        let window_secs = settings.window_secs;
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
}

pin_project_lite::pin_project! {
    /// Future returned by [`ReadYourWritesService`]: the inner service's future
    /// polled inside the task-local pin scope, then the session-mode
    /// `Set-Cookie` stamp on the way out.
    pub(crate) struct ReadYourWritesFuture<F> {
        #[pin]
        inner: tokio::task::futures::TaskLocalFuture<RequestPin, F>,
        settings: Arc<ReadYourWritesSettings>,
        pin: RequestPin,
    }
}

impl<F, E> std::future::Future for ReadYourWritesFuture<F>
where
    F: std::future::Future<Output = Result<axum::response::Response, E>>,
{
    type Output = Result<axum::response::Response, E>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let this = self.project();
        let mut response = std::task::ready!(this.inner.poll(cx))?;
        stamp_ryw_cookie(&mut response, this.settings, this.pin);
        std::task::Poll::Ready(Ok(response))
    }
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

/// Direct coverage for [`ReadYourWritesService`] (issue #2214).
///
/// The framework's ingress installs the *layer*; the retained [`middleware`]
/// `async fn` is what the pre-existing `middleware() integration tests` in this
/// file exercise, through an `axum::middleware::from_fn`. Nothing drove the
/// service form, so the two properties that live in the wiring rather than in
/// the shared helpers — the task-local pin being installed, and the response-side
/// cookie stamp — were untested on the path production actually takes.
#[cfg(test)]
mod service_tests {
    use super::{ReadYourWritesLayer, is_pinned, mark_write};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use axum::body::Body;
    use axum::http::{Request, Response, StatusCode};
    use tower::{Layer, Service, ServiceExt};

    use crate::config::ReadYourWrites;

    fn test_keys() -> Arc<crate::security::config::ResolvedSigningKeys> {
        Arc::new(crate::security::config::ResolvedSigningKeys::new(
            b"test-key-for-ryw-service".to_vec(),
            vec![],
        ))
    }

    /// Inner service that marks a primary write (the way a generated mutating
    /// repository method does) and reports whether the pin was visible.
    fn writing_handler(
        saw_scope: Arc<AtomicBool>,
        write: bool,
    ) -> impl Service<Request<Body>, Response = Response<Body>, Error = std::convert::Infallible> + Clone
    {
        tower::service_fn(move |_req: Request<Body>| {
            let saw_scope = Arc::clone(&saw_scope);
            async move {
                if write {
                    mark_write();
                }
                saw_scope.store(is_pinned(), Ordering::SeqCst);
                Ok::<_, std::convert::Infallible>(
                    Response::builder()
                        .status(StatusCode::OK)
                        .body(Body::empty())
                        .expect("response builds"),
                )
            }
        })
    }

    /// `#[allow(future_not_send)]`: `tower::service_fn`'s closure is not
    /// `Sync`, so this helper's future is `!Send`. It only ever runs on a
    /// `#[tokio::test]` current-thread runtime, which never moves it.
    #[allow(clippy::future_not_send)]
    async fn run(
        mode: ReadYourWrites,
        keys: Option<Arc<crate::security::config::ResolvedSigningKeys>>,
        write: bool,
    ) -> (bool, Option<String>) {
        let saw_scope = Arc::new(AtomicBool::new(false));
        let layer = ReadYourWritesLayer::new(
            mode,
            60,
            keys,
            crate::middleware::MetricsCollector::default(),
        );
        let response = layer
            .layer(writing_handler(Arc::clone(&saw_scope), write))
            .oneshot(Request::builder().body(Body::empty()).expect("request"))
            .await
            .expect("infallible");
        let cookie = response
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .map(|v| v.to_str().expect("ascii cookie").to_owned());
        (saw_scope.load(Ordering::SeqCst), cookie)
    }

    /// The whole reason this layer exists: the request-scoped pin must be
    /// visible to everything beneath it. Without the task-local scope,
    /// `is_pinned()` reads `false` after a write and every replica-eligible read
    /// silently goes to the replica.
    #[tokio::test]
    async fn the_request_pin_scope_is_installed_for_the_inner_service() {
        let (saw_scope, _) = run(ReadYourWrites::Request, None, true).await;
        assert!(
            saw_scope,
            "a write inside the handler must leave `is_pinned()` true — the \
             task-local pin scope is not reaching the inner service"
        );
    }

    /// Control: no write, no pin. Proves the assertion above tracks the write
    /// rather than being true for every request.
    #[tokio::test]
    async fn no_write_means_no_pin() {
        let (saw_scope, _) = run(ReadYourWrites::Request, None, false).await;
        assert!(!saw_scope);
    }

    /// Session mode stamps the signed `autumn.ryw` cookie after a write, with
    /// the documented attributes, so the *next* request also routes to primary.
    #[tokio::test]
    async fn session_mode_stamps_the_ryw_cookie_after_a_write() {
        let (_, cookie) = run(ReadYourWrites::Session, Some(test_keys()), true).await;
        let cookie = cookie.expect("session mode must stamp autumn.ryw after a write");
        assert!(cookie.starts_with("autumn.ryw="), "got {cookie}");
        assert!(cookie.contains("Max-Age=60"), "got {cookie}");
        assert!(cookie.contains("HttpOnly"), "got {cookie}");
        assert!(cookie.contains("Secure"), "got {cookie}");
        assert!(cookie.contains("SameSite=Lax"), "got {cookie}");
        assert!(cookie.contains("Path=/"), "got {cookie}");
    }

    /// No write, no cookie — the freshness window is only opened by an actual
    /// primary write.
    #[tokio::test]
    async fn session_mode_stamps_no_cookie_without_a_write() {
        let (_, cookie) = run(ReadYourWrites::Session, Some(test_keys()), false).await;
        assert_eq!(cookie, None);
    }

    /// Request mode is per-request only: it never mints a cross-request cookie,
    /// even after a write.
    #[tokio::test]
    async fn request_mode_never_stamps_a_cookie() {
        let (_, cookie) = run(ReadYourWrites::Request, Some(test_keys()), true).await;
        assert_eq!(cookie, None);
    }

    /// Session mode with no signing key configured cannot sign the cookie, so
    /// it stamps none — the warn-and-degrade path `apply_middleware` documents.
    #[tokio::test]
    async fn session_mode_without_keys_stamps_no_cookie() {
        let (saw_scope, cookie) = run(ReadYourWrites::Session, None, true).await;
        assert_eq!(cookie, None);
        assert!(saw_scope, "in-request pinning still works without a key");
    }
}
