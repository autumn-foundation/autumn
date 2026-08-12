//! The request-scoped capture buffer and the Tower layer that establishes it.
//!
//! [`CaptureLayer`] sits outer to
//! [`ReportingLayer`](crate::reporting::ReportingLayer) — the failure is not
//! known yet when the request arrives, so every request gets a
//! [`CaptureScope`] and the reporting layer decides at the end whether it is
//! worth writing. A scope is reachable two ways while the handler runs:
//!
//! * through the [`CAPSULE_SCOPE`] task-local, for effect sources deep in the
//!   stack that have no handle to thread (the clock);
//! * through a [`CaptureHandle`] in the request extensions, for the reporting
//!   layer, which must keep the scope alive across a panic unwind.
//!
//! Database recording cannot use the task-local (the pooled connection's I/O
//! runs on its own task), so scopes are additionally published in a
//! weak-reference registry keyed by capsule id; the connection recorder looks
//! its scope up by the id it read off the `SET autumn.capsule_request` marker.

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

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, Weak};
use std::task::{Context, Poll};

use axum::body::Body;
use axum::extract::MatchedPath;
use axum::http::{Request, Response};
use chrono::{DateTime, Utc};
use tower::{Layer, Service};

use crate::capsule::redact::{CapturedBody, RawRequest};
use crate::capsule::schema::{CapsuleDb, ConnectionTape};
use crate::log::filter::ParameterFilter;

tokio::task_local! {
    /// The capture scope of the request currently being served on this task.
    pub static CAPSULE_SCOPE: Arc<CaptureScope>;
}

/// The capture scope of the request being served on this task, if any.
#[must_use]
pub fn current_scope() -> Option<Arc<CaptureScope>> {
    CAPSULE_SCOPE.try_with(Arc::clone).ok()
}

/// Whether database effect capture is armed for this process.
///
/// Read on the connection-checkout hot path, so it is a plain relaxed atomic
/// load rather than a config lookup.
static DB_CAPTURE_ENABLED: AtomicBool = AtomicBool::new(false);

/// Whether the connection-checkout path should attribute queries to a capsule.
#[must_use]
pub fn db_capture_enabled() -> bool {
    DB_CAPTURE_ENABLED.load(Ordering::Relaxed)
}

/// Arm or disarm process-wide capture from the resolved configuration.
///
/// Idempotent and last-writer-wins. Called early in
/// [`App::run`](crate::app::AppBuilder::run) — before the database pool is
/// built, because the pool factory consults it — and again at router-build
/// time so test apps observe the same wiring.
pub fn install_from_config(enabled: bool) {
    DB_CAPTURE_ENABLED.store(enabled, Ordering::Relaxed);
}

/// Immutable knobs a scope needs to bound and place its capsule.
#[derive(Debug, Clone)]
pub struct CaptureSettings {
    /// Directory capsules are written to.
    pub dir: String,
    /// Largest request body copied into a capsule.
    pub max_body_bytes: usize,
    /// Size ceiling for recorded effects before a capsule is marked truncated.
    pub max_capsule_bytes: usize,
    /// How many capsules to retain before pruning oldest-first.
    pub max_capsules: usize,
    /// Recording application's name, for cross-build mismatch warnings.
    pub app_name: Option<String>,
    /// Recording application's active profile.
    pub profile: Option<String>,
}

impl Default for CaptureSettings {
    fn default() -> Self {
        Self {
            dir: "tmp/autumn-capsules".to_owned(),
            max_body_bytes: 65_536,
            max_capsule_bytes: 1_048_576,
            max_capsules: 50,
            app_name: None,
            profile: None,
        }
    }
}

/// Recorded database traffic for one request, keyed by connection.
///
/// The connection recorder owns the contents; this type only provides the
/// per-request accumulation and the byte budget that stops an unbounded query
/// result from filling memory.
///
/// Tapes are kept in the order the request **first used** each connection, not
/// by connection id. Ids are process-wide birth order and say nothing about
/// this request: a long-lived pooled connection can carry a much lower id than
/// one minted moments ago, so a request that used the fresh connection first
/// would have its tapes listed backwards. Replay hands tape *i* to the *i*-th
/// connection its pool opens (F12), so a reordering there swaps the tapes and
/// makes both connections diverge against traffic that was recorded perfectly.
#[derive(Debug, Default)]
pub struct DbBuffer {
    tapes: BTreeMap<u64, ConnectionTape>,
    /// Connection ids in first-use order — the order [`snapshot`](Self::snapshot)
    /// writes them, and therefore the order replay claims them in.
    order: Vec<u64>,
    bytes: usize,
}

impl DbBuffer {
    /// The tape for a connection, created on first use — and remembered in
    /// `order` at that moment, which is what makes the snapshot first-use
    /// ordered.
    pub fn tape_mut(&mut self, connection_id: u64) -> &mut ConnectionTape {
        match self.tapes.entry(connection_id) {
            std::collections::btree_map::Entry::Occupied(tape) => tape.into_mut(),
            std::collections::btree_map::Entry::Vacant(slot) => {
                self.order.push(connection_id);
                slot.insert(ConnectionTape {
                    id: connection_id,
                    ..ConnectionTape::default()
                })
            }
        }
    }

    /// Charge `bytes` against the capsule budget.
    ///
    /// Returns `false` once the budget is exhausted, at which point the caller
    /// must stop recording and mark the capsule truncated.
    pub const fn charge(&mut self, bytes: usize, budget: usize) -> bool {
        self.bytes = self.bytes.saturating_add(bytes);
        self.bytes <= budget
    }

    /// Bytes charged so far.
    #[must_use]
    pub const fn charged_bytes(&self) -> usize {
        self.bytes
    }

    /// Whether any traffic was recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tapes.is_empty()
    }

    /// Snapshot the tapes for serialization, in first-use order.
    #[must_use]
    pub fn snapshot(&self) -> Option<CapsuleDb> {
        if self.tapes.is_empty() {
            return None;
        }
        Some(CapsuleDb {
            connections: self
                .order
                .iter()
                .filter_map(|id| self.tapes.get(id))
                .cloned()
                .collect(),
        })
    }
}

/// Most clock readings one capsule will hold.
const MAX_CLOCK_READINGS: usize = 10_000;

/// How the capture layer is treating one request's body.
///
/// The layer never reads the body itself (see [`TeeBody`]), so this is the
/// running state of a copy the *handler* is driving.
#[derive(Debug, Default)]
enum BodyTap {
    /// Nothing to copy: no body was declared, or it was declared empty.
    #[default]
    Absent,
    /// Deliberately not copied — the declared length was over the cap, so the
    /// body streams to the handler untouched.
    Skipped {
        /// Length the client declared, when it declared one.
        declared_len: Option<usize>,
    },
    /// Being copied frame by frame as the handler reads it.
    Teeing {
        /// Length the client declared, when it declared one.
        declared_len: Option<usize>,
        /// Bytes copied so far.
        buf: Vec<u8>,
        /// Whether the handler read the body all the way to its end.
        end_stream: bool,
        /// Whether the copy was abandoned for exceeding `max_body_bytes`.
        overflowed: bool,
    },
}

/// Note recorded when a streaming body grew past `max_body_bytes`.
const BODY_OVERFLOW_NOTE: &str =
    "request body exceeded max_body_bytes while streaming; it was not captured";

/// Note recorded when the handler stopped reading the body before its end.
const BODY_PARTIAL_NOTE: &str =
    "request body was not read to its end before the failure; the captured body is incomplete";

/// Everything one in-flight request has offered up for its capsule.
#[derive(Debug)]
pub struct CaptureScope {
    id: String,
    settings: Arc<CaptureSettings>,
    filter: Arc<ParameterFilter>,
    request: OnceLock<RawRequest>,
    body: Mutex<BodyTap>,
    clock: Mutex<Vec<DateTime<Utc>>>,
    db: Mutex<DbBuffer>,
    notes: Mutex<Vec<String>>,
    truncated: AtomicBool,
    closed: AtomicBool,
}

impl CaptureScope {
    /// Create a scope for a request.
    #[must_use]
    pub fn new(id: String, settings: Arc<CaptureSettings>, filter: Arc<ParameterFilter>) -> Self {
        Self {
            id,
            settings,
            filter,
            request: OnceLock::new(),
            body: Mutex::new(BodyTap::Absent),
            clock: Mutex::new(Vec::new()),
            db: Mutex::new(DbBuffer::default()),
            notes: Mutex::new(Vec::new()),
            truncated: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        }
    }

    /// The capsule id (the request id, when one was available).
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The knobs this scope was built with.
    #[must_use]
    pub fn settings(&self) -> &CaptureSettings {
        &self.settings
    }

    /// The redaction filter this scope's capsule must be written through.
    #[must_use]
    pub fn filter(&self) -> &ParameterFilter {
        &self.filter
    }

    /// Record the unredacted request snapshot. Only the first call takes.
    pub fn set_request(&self, request: RawRequest) {
        let _ = self.request.set(request);
    }

    /// The unredacted request snapshot, if the layer recorded one.
    ///
    /// The snapshot is the request *head* only; the body arrives separately
    /// through [`captured_body`](Self::captured_body), because it is copied
    /// while the handler reads it rather than up front.
    #[must_use]
    pub fn raw_request(&self) -> Option<&RawRequest> {
        self.request.get()
    }

    /// Decide how this request's body will be treated, before the handler runs.
    fn arm_body(&self, tap: BodyTap) {
        if let Ok(mut current) = self.body.lock() {
            *current = tap;
        }
    }

    /// Copy a data frame the handler has just read.
    ///
    /// Bounded by `max_body_bytes`: the frame that would cross the cap ends
    /// the copy and releases what was collected, so an unexpectedly large
    /// streamed upload cannot be buffered in memory by the mere presence of
    /// capture.
    fn tee_body_chunk(&self, chunk: &[u8]) {
        let limit = self.settings.max_body_bytes;
        if let Ok(mut tap) = self.body.lock()
            && let BodyTap::Teeing {
                buf, overflowed, ..
            } = &mut *tap
            && !*overflowed
        {
            if buf.len().saturating_add(chunk.len()) > limit {
                *overflowed = true;
                *buf = Vec::new();
            } else {
                buf.extend_from_slice(chunk);
            }
        }
    }

    /// Record that the handler read the body all the way to its end.
    fn mark_body_end(&self) {
        if let Ok(mut tap) = self.body.lock()
            && let BodyTap::Teeing { end_stream, .. } = &mut *tap
        {
            *end_stream = true;
        }
    }

    /// The request body, as far as the handler read it before the failure.
    #[must_use]
    pub fn captured_body(&self) -> CapturedBody {
        let Ok(tap) = self.body.lock() else {
            return CapturedBody::Absent;
        };
        match &*tap {
            BodyTap::Absent => CapturedBody::Absent,
            // Declared over the cap up front, or discovered to be over it
            // mid-stream: either way the capsule records a skip, not a body.
            BodyTap::Skipped { declared_len }
            | BodyTap::Teeing {
                declared_len,
                overflowed: true,
                ..
            } => CapturedBody::Skipped {
                declared_len: *declared_len,
            },
            BodyTap::Teeing { buf, .. } if buf.is_empty() => CapturedBody::Absent,
            BodyTap::Teeing { buf, .. } => CapturedBody::Buffered(bytes::Bytes::from(buf.clone())),
        }
    }

    /// A note explaining a body the capsule reader should not trust as
    /// complete, if this request produced one.
    #[must_use]
    pub fn body_note(&self) -> Option<&'static str> {
        let tap = self.body.lock().ok()?;
        match &*tap {
            BodyTap::Teeing {
                overflowed: true, ..
            } => Some(BODY_OVERFLOW_NOTE),
            BodyTap::Teeing {
                end_stream: false, ..
            } => Some(BODY_PARTIAL_NOTE),
            _ => None,
        }
    }

    /// Append a clock reading.
    ///
    /// Bounded: a pathological loop reading `now()` must not grow the buffer
    /// without limit, and a capsule that long is not replayable anyway.
    pub fn record_clock(&self, reading: DateTime<Utc>) {
        if let Ok(mut readings) = self.clock.lock() {
            if readings.len() >= MAX_CLOCK_READINGS {
                self.truncated.store(true, Ordering::Relaxed);
                return;
            }
            readings.push(reading);
        }
    }

    /// The clock readings taken during the request, in order.
    #[must_use]
    pub fn clock_readings(&self) -> Vec<DateTime<Utc>> {
        self.clock
            .lock()
            .map(|readings| readings.clone())
            .unwrap_or_default()
    }

    /// Operate on the recorded database traffic.
    pub fn with_db<R>(&self, f: impl FnOnce(&mut DbBuffer) -> R) -> Option<R> {
        self.db.lock().ok().map(|mut db| f(&mut db))
    }

    /// Snapshot the recorded database traffic for serialization.
    #[must_use]
    pub fn db_snapshot(&self) -> Option<CapsuleDb> {
        self.db.lock().ok().and_then(|db| db.snapshot())
    }

    /// Note a degraded-capture condition for the capsule reader.
    pub fn note(&self, note: impl Into<String>) {
        let note = note.into();
        if let Ok(mut notes) = self.notes.lock()
            && !notes.contains(&note)
        {
            notes.push(note);
        }
    }

    /// The accumulated notes.
    #[must_use]
    pub fn notes(&self) -> Vec<String> {
        self.notes
            .lock()
            .map(|notes| notes.clone())
            .unwrap_or_default()
    }

    /// Stop accepting effects: the request this scope belongs to is over.
    ///
    /// Called when the capture layer's future resolves — normally or through a
    /// panic unwind. The scope itself lives on until the capsule is written,
    /// so this is what stops *late* effects from joining it. Chiefly the
    /// connection pool's liveness check: `pool.get()` pings the connection
    /// before [`Db::checkout`](crate::db::Db::checkout) sends the next
    /// request's attribution marker, so without a close the ping would be
    /// recorded against whoever held that connection last, and replay of that
    /// capsule would then expect a query its handler never issued (F2).
    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
    }

    /// Whether the request is over and the capsule is no longer accepting
    /// effects.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// Mark the capsule as incomplete; replay must refuse it.
    pub fn mark_truncated(&self) {
        self.truncated.store(true, Ordering::Relaxed);
    }

    /// Whether a size cap stopped recording partway through.
    #[must_use]
    pub fn is_truncated(&self) -> bool {
        self.truncated.load(Ordering::Relaxed)
    }
}

/// A cloneable handle to a request's [`CaptureScope`], carried in the request
/// extensions so the reporting layer can reach it after an unwind.
#[derive(Clone, Debug)]
pub struct CaptureHandle(Arc<CaptureScope>);

impl CaptureHandle {
    /// The scope this handle keeps alive.
    #[must_use]
    pub const fn scope(&self) -> &Arc<CaptureScope> {
        &self.0
    }
}

// ── Registry ────────────────────────────────────────────────────────────────

/// Live scopes by capsule id, weakly held so a finished request's scope is
/// freed even if deregistration is skipped.
static REGISTRY: LazyLock<Mutex<HashMap<String, Weak<CaptureScope>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Look a live scope up by the capsule id a connection marker carried.
#[must_use]
pub fn scope_by_id(id: &str) -> Option<Arc<CaptureScope>> {
    REGISTRY
        .lock()
        .ok()
        .and_then(|registry| registry.get(id).and_then(Weak::upgrade))
}

fn register(scope: &Arc<CaptureScope>) {
    if let Ok(mut registry) = REGISTRY.lock() {
        registry.insert(scope.id().to_owned(), Arc::downgrade(scope));
    }
}

fn deregister(id: &str) {
    if let Ok(mut registry) = REGISTRY.lock() {
        registry.remove(id);
    }
}

/// Closes a scope and removes it from the registry when the request's future
/// is dropped, including when it is dropped by a panic unwind.
struct RegistryGuard(Arc<CaptureScope>);

impl Drop for RegistryGuard {
    fn drop(&mut self) {
        // Order matters: closing first means a connection recorder that is
        // mid-append when the request ends cannot slip an effect in between
        // the two steps.
        self.0.close();
        deregister(self.0.id());
    }
}

// ── Scope id ────────────────────────────────────────────────────────────────

/// Longest capsule id accepted; the id is interpolated into the `SET
/// autumn.capsule_request` marker, so it is length- and charset-bounded.
const MAX_SCOPE_ID_LEN: usize = 64;

/// Whether an id is safe to interpolate into the connection marker SQL.
#[must_use]
pub fn is_valid_scope_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_SCOPE_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

// ── Tower layer ─────────────────────────────────────────────────────────────

/// Tower [`Layer`] that establishes a [`CaptureScope`] for every request.
///
/// Installed only when `[failure_capture] enabled = true`, immediately outer to
/// [`ReportingLayer`](crate::reporting::ReportingLayer) so a scope exists
/// before the reporting layer snapshots its request context.
#[derive(Clone)]
pub struct CaptureLayer {
    settings: Arc<CaptureSettings>,
    filter: Arc<ParameterFilter>,
}

impl CaptureLayer {
    /// Build the layer from resolved settings and the shared redaction filter.
    #[must_use]
    pub fn new(settings: CaptureSettings, filter: Arc<ParameterFilter>) -> Self {
        Self {
            settings: Arc::new(settings),
            filter,
        }
    }
}

impl<S> Layer<S> for CaptureLayer {
    type Service = CaptureService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        CaptureService {
            inner,
            settings: Arc::clone(&self.settings),
            filter: Arc::clone(&self.filter),
        }
    }
}

/// Tower [`Service`] produced by [`CaptureLayer`].
#[derive(Clone)]
pub struct CaptureService<S> {
    inner: S,
    settings: Arc<CaptureSettings>,
    filter: Arc<ParameterFilter>,
}

impl<S> Service<Request<Body>> for CaptureService<S>
where
    S: Service<Request<Body>, Response = Response<Body>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
{
    type Response = Response<Body>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        // Clone-and-replace so the polled-ready service moves into the future.
        let cloned = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, cloned);
        let settings = Arc::clone(&self.settings);
        let filter = Arc::clone(&self.filter);

        Box::pin(async move {
            let id = scope_id(&req);
            let route = req
                .extensions()
                .get::<MatchedPath>()
                .map(|matched| matched.as_str().to_owned());
            let scope = Arc::new(CaptureScope::new(id, settings, filter));
            scope.set_request(RawRequest {
                method: req.method().as_str().to_owned(),
                uri: req.uri().clone(),
                version: req.version(),
                headers: req.headers().clone(),
                route,
            });
            // Note the body is *teed*, never pre-read: see `arm_body_capture`.
            let mut req = arm_body_capture(req, &scope);
            register(&scope);
            let _guard = RegistryGuard(Arc::clone(&scope));
            req.extensions_mut()
                .insert(CaptureHandle(Arc::clone(&scope)));

            // The scope ends when the response future resolves, so effects a
            // streaming body produces afterwards are not captured — everything
            // a failure needs happens during handler execution.
            CAPSULE_SCOPE.scope(scope, inner.call(req)).await
        })
    }
}

/// The capsule id for a request: its request id when
/// [`RequestIdLayer`](crate::middleware::RequestIdLayer) (installed outer to
/// this one) has already assigned one, else a fresh id.
fn scope_id(req: &Request<Body>) -> String {
    req.extensions()
        .get::<crate::middleware::RequestId>()
        .map(std::string::ToString::to_string)
        .filter(|id| is_valid_scope_id(id))
        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string())
}

/// Arrange for the request body to be copied into the scope *as the handler
/// reads it*, rather than buffered here.
///
/// This layer is installed outer to the request-timeout layer (see the layer
/// order in `router.rs`), so anything it reads off the socket is read before
/// the deadline starts: pre-buffering even a small body would let a client
/// drip-feed it forever and hold a worker open — a slow-loris vector that
/// would exist only when capture is enabled. Teeing leaves the read where it
/// belongs, inside the handler, where the timeout already bounds it.
///
/// A body declared larger than `max_body_bytes` is not wrapped at all, so an
/// upload streams to the handler exactly as it would without capture.
fn arm_body_capture(req: Request<Body>, scope: &Arc<CaptureScope>) -> Request<Body> {
    let max_body_bytes = scope.settings().max_body_bytes;
    let declared_len = body_length(&req);

    match declared_len {
        Some(0) => {
            scope.arm_body(BodyTap::Absent);
            req
        }
        None if !has_undeclared_body(&req) => {
            scope.arm_body(BodyTap::Absent);
            req
        }
        Some(len) if len > max_body_bytes => {
            scope.arm_body(BodyTap::Skipped { declared_len });
            req
        }
        _ => {
            scope.arm_body(BodyTap::Teeing {
                declared_len,
                buf: Vec::new(),
                end_stream: false,
                overflowed: false,
            });
            let (parts, body) = req.into_parts();
            let teed = Body::new(TeeBody {
                inner: body,
                scope: Arc::clone(scope),
            });
            Request::from_parts(parts, teed)
        }
    }
}

/// Request body that copies each data frame into the capture scope on its way
/// through to the handler.
///
/// Every method delegates, so the handler sees the body it would have seen
/// without capture — same frames, same order, same end-of-stream, same size
/// hint. The copy is bounded by `max_body_bytes` and stops there.
struct TeeBody {
    inner: Body,
    scope: Arc<CaptureScope>,
}

impl http_body::Body for TeeBody {
    type Data = bytes::Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.inner).poll_frame(cx);
        match &polled {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.scope.tee_body_chunk(data);
                }
            }
            // The handler read the body to its end: what was copied is whole.
            Poll::Ready(None) => this.scope.mark_body_end(),
            Poll::Ready(Some(Err(_))) | Poll::Pending => {}
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        http_body::Body::size_hint(&self.inner)
    }
}

/// The request body's length, from `Content-Length` or — for a body already in
/// memory, as an in-process test client or a body-buffering outer layer
/// produces — the body's own exact size hint.
fn body_length(req: &Request<Body>) -> Option<usize> {
    req.headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .or_else(|| usize::try_from(http_body::Body::size_hint(req.body()).exact()?).ok())
}

/// Whether a request whose length [`body_length`] could not determine still has
/// a body worth teeing.
///
/// Asked of the **body**, not the headers. `Transfer-Encoding: chunked` is the
/// HTTP/1.1 way of announcing a body of unknown length, but HTTP/2 and HTTP/3
/// have no such header: a streamed h2 request arrives with no `Content-Length`,
/// no exact size hint and nothing in the headers to go on. Requiring the header
/// would classify every one of those as having no body at all, and the capsule
/// would replay the request empty — a silent falsification, not a limitation.
///
/// Only a body already at end-of-stream is treated as absent. Anything else is
/// teed: `max_body_bytes` still bounds what is kept, and a body that turns out
/// to be empty snapshots as [`CapturedBody::Absent`] anyway, so guessing "yes"
/// costs a wrapper and never a wrong capsule.
fn has_undeclared_body(req: &Request<Body>) -> bool {
    !http_body::Body::is_end_stream(req.body())
}

#[cfg(test)]
mod tests {
    use super::*;

    use bytes::Bytes;
    use http_body::Frame;

    /// Ordered log of who touched what, shared between a test body and the
    /// service it is sent through.
    #[derive(Clone, Default)]
    struct Trace(Arc<Mutex<Vec<&'static str>>>);

    impl Trace {
        fn record(&self, what: &'static str) {
            if let Ok(mut entries) = self.0.lock() {
                entries.push(what);
            }
        }

        fn entries(&self) -> Vec<&'static str> {
            self.0
                .lock()
                .map(|entries| entries.clone())
                .unwrap_or_default()
        }
    }

    /// A request body that logs every poll, so a test can see exactly when the
    /// bytes were read relative to the handler running.
    struct WatchedBody {
        trace: Trace,
        chunks: Vec<&'static [u8]>,
    }

    impl http_body::Body for WatchedBody {
        type Data = Bytes;
        type Error = axum::Error;

        fn poll_frame(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
            let this = self.get_mut();
            this.trace.record("body-polled");
            if this.chunks.is_empty() {
                Poll::Ready(None)
            } else {
                let chunk = this.chunks.remove(0);
                Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(chunk)))))
            }
        }
    }

    fn test_layer(settings: CaptureSettings) -> CaptureLayer {
        CaptureLayer::new(settings, Arc::new(ParameterFilter::new(&[], &[])))
    }

    /// Run a request through the capture layer with an inner service that
    /// records its entry, optionally drains the body, and hands back the
    /// capture handle it found in the extensions.
    async fn run_capture(
        settings: CaptureSettings,
        request: Request<Body>,
        trace: Trace,
        read_body: bool,
    ) -> Arc<CaptureScope> {
        let seen: Arc<Mutex<Option<CaptureHandle>>> = Arc::new(Mutex::new(None));
        let inner_seen = Arc::clone(&seen);
        let inner_trace = trace.clone();
        let inner = tower::service_fn(move |req: Request<Body>| {
            let seen = Arc::clone(&inner_seen);
            let trace = inner_trace.clone();
            async move {
                trace.record("inner-called");
                if let Some(handle) = req.extensions().get::<CaptureHandle>().cloned()
                    && let Ok(mut slot) = seen.lock()
                {
                    *slot = Some(handle);
                }
                if read_body {
                    let _ = axum::body::to_bytes(req.into_body(), usize::MAX).await;
                    trace.record("handler-read-body");
                }
                Ok::<_, std::convert::Infallible>(Response::new(Body::empty()))
            }
        });

        let mut service = test_layer(settings).layer(inner);
        let _response = service
            .call(request)
            .await
            .expect("inner service is infallible");
        let handle = seen
            .lock()
            .expect("handle slot")
            .clone()
            .expect("the capture layer must publish a handle in the request extensions");
        Arc::clone(handle.scope())
    }

    #[tokio::test]
    async fn call_does_not_read_the_body_before_the_inner_service_runs() {
        // The capture layer sits *outside* the request-timeout layer, so any
        // byte it reads off the socket itself is read before the deadline
        // starts. A slow client dripping a small body would otherwise hold a
        // worker open forever. Bytes must only be read by the handler.
        let trace = Trace::default();
        let request = Request::post("/x")
            .header(axum::http::header::CONTENT_LENGTH, "7")
            .body(Body::new(WatchedBody {
                trace: trace.clone(),
                chunks: vec![b"payload"],
            }))
            .expect("request builds");

        let _scope = run_capture(CaptureSettings::default(), request, trace.clone(), true).await;

        let entries = trace.entries();
        assert_eq!(
            entries.first(),
            Some(&"inner-called"),
            "capture must not touch the request body before the inner service \
             (and therefore the request timeout) is running, got {entries:?}"
        );
    }

    #[tokio::test]
    async fn teed_body_is_captured_whole_when_the_handler_reads_it() {
        let trace = Trace::default();
        let request = Request::post("/x")
            .header(axum::http::header::CONTENT_LENGTH, "10")
            .body(Body::new(WatchedBody {
                trace: trace.clone(),
                chunks: vec![b"hello", b"world"],
            }))
            .expect("request builds");

        let scope = run_capture(CaptureSettings::default(), request, trace, true).await;

        match scope.captured_body() {
            CapturedBody::Buffered(bytes) => assert_eq!(&bytes[..], b"helloworld"),
            other => panic!("a fully read body must be captured whole, got {other:?}"),
        }
        assert_eq!(
            scope.body_note(),
            None,
            "a complete body needs no caveat in the capsule"
        );
    }

    #[tokio::test]
    async fn body_the_handler_never_reads_leaves_a_note_not_a_capture() {
        let trace = Trace::default();
        let request = Request::post("/x")
            .header(axum::http::header::CONTENT_LENGTH, "7")
            .body(Body::new(WatchedBody {
                trace: trace.clone(),
                chunks: vec![b"payload"],
            }))
            .expect("request builds");

        let scope = run_capture(CaptureSettings::default(), request, trace.clone(), false).await;

        assert!(
            !trace.entries().contains(&"body-polled"),
            "nothing may read a body the handler ignored, got {:?}",
            trace.entries()
        );
        assert!(matches!(scope.captured_body(), CapturedBody::Absent));
        assert_eq!(
            scope.body_note(),
            Some(BODY_PARTIAL_NOTE),
            "the capsule must say the body is incomplete rather than imply the \
             request had none"
        );
    }

    #[tokio::test]
    async fn streamed_body_with_no_length_and_no_transfer_encoding_is_still_teed() {
        // HTTP/2 (and /3) have no `Transfer-Encoding`: a streamed h2 request
        // arrives with no `Content-Length`, no exact size hint and no header to
        // hint at one. Deciding "does this request have a body" from the
        // HTTP/1.1 header alone classifies it as having none, and the capsule
        // then replays a request whose body has silently vanished.
        let trace = Trace::default();
        let request = Request::post("/x")
            .version(axum::http::Version::HTTP_2)
            .body(Body::new(WatchedBody {
                trace: trace.clone(),
                chunks: vec![b"h2-", b"payload"],
            }))
            .expect("request builds");

        let scope = run_capture(CaptureSettings::default(), request, trace, true).await;

        match scope.captured_body() {
            CapturedBody::Buffered(bytes) => assert_eq!(&bytes[..], b"h2-payload"),
            other => panic!(
                "a body with no declared length must be teed, not assumed absent, got {other:?}"
            ),
        }
    }

    #[tokio::test]
    async fn a_request_with_no_body_at_all_is_recorded_as_absent() {
        // The other side of the same predicate: an empty body is at
        // end-of-stream from the start, so nothing is wrapped and the capsule
        // records a request that genuinely had no body.
        let request = Request::get("/x")
            .body(Body::empty())
            .expect("request builds");
        let scope = run_capture(CaptureSettings::default(), request, Trace::default(), true).await;

        assert!(matches!(scope.captured_body(), CapturedBody::Absent));
        assert_eq!(
            scope.body_note(),
            None,
            "a request with no body needs no caveat"
        );
    }

    #[tokio::test]
    async fn streamed_body_over_the_cap_is_dropped_mid_stream() {
        // No declared length, so the layer cannot skip up front: the cap has to
        // hold while the frames arrive.
        let trace = Trace::default();
        let request = Request::post("/x")
            .header(axum::http::header::TRANSFER_ENCODING, "chunked")
            .body(Body::new(WatchedBody {
                trace: trace.clone(),
                chunks: vec![b"1234", b"5678", b"9012"],
            }))
            .expect("request builds");

        let settings = CaptureSettings {
            max_body_bytes: 6,
            ..CaptureSettings::default()
        };
        let scope = run_capture(settings, request, trace, true).await;

        assert!(
            matches!(
                scope.captured_body(),
                CapturedBody::Skipped { declared_len: None }
            ),
            "a body that outgrows the cap mid-stream must be dropped, got {:?}",
            scope.captured_body()
        );
        assert_eq!(scope.body_note(), Some(BODY_OVERFLOW_NOTE));
    }

    #[tokio::test]
    async fn body_declared_over_the_cap_is_never_wrapped() {
        let trace = Trace::default();
        let request = Request::post("/x")
            .header(axum::http::header::CONTENT_LENGTH, "4096")
            .body(Body::new(WatchedBody {
                trace: trace.clone(),
                chunks: vec![b"1234"],
            }))
            .expect("request builds");

        let settings = CaptureSettings {
            max_body_bytes: 16,
            ..CaptureSettings::default()
        };
        let scope = run_capture(settings, request, trace, true).await;

        assert!(
            matches!(
                scope.captured_body(),
                CapturedBody::Skipped {
                    declared_len: Some(4096)
                }
            ),
            "an oversized upload must be recorded as skipped, got {:?}",
            scope.captured_body()
        );
        assert_eq!(
            scope.body_note(),
            None,
            "skipping a declared-oversized body is the documented behaviour, \
             not a degraded capture"
        );
    }

    #[tokio::test]
    async fn the_scope_closes_when_the_request_ends() {
        // The scope outlives the request — the reporting layer writes the
        // capsule from a detached task — so "closed" is what stops a pooled
        // connection's next liveness ping from being recorded as something
        // this request did.
        let request = Request::get("/x")
            .body(Body::empty())
            .expect("request builds");
        let scope = run_capture(CaptureSettings::default(), request, Trace::default(), false).await;

        assert!(
            scope.is_closed(),
            "a finished request must stop accepting effects"
        );
        assert!(
            scope_by_id(scope.id()).is_none(),
            "and must no longer be reachable by a connection marker"
        );
    }

    #[test]
    fn scope_ids_are_bounded_and_charset_checked() {
        assert!(is_valid_scope_id("018f-4b2c_AB"));
        assert!(!is_valid_scope_id(""));
        assert!(!is_valid_scope_id("has space"));
        assert!(!is_valid_scope_id("quote'; DROP TABLE users; --"));
        assert!(!is_valid_scope_id(&"a".repeat(MAX_SCOPE_ID_LEN + 1)));
    }

    #[test]
    fn db_buffer_charges_against_the_budget() {
        let mut buffer = DbBuffer::default();
        assert!(buffer.charge(400, 1000));
        assert!(buffer.charge(600, 1000));
        assert!(!buffer.charge(1, 1000), "the budget must eventually stop");
        assert_eq!(buffer.charged_bytes(), 1001);
    }

    #[test]
    fn db_buffer_snapshots_tapes_in_first_use_order() {
        // Connection ids are process-wide birth order, which says nothing about
        // the order *this* request reached for them: a long-lived pooled
        // connection 2 can easily be checked out after a freshly minted 7.
        // Replay hands tape *i* to the *i*-th connection its pool opens, so the
        // capsule must list them in the order the request first used them or
        // the tapes get swapped and both connections diverge.
        let mut buffer = DbBuffer::default();
        buffer.tape_mut(7);
        buffer.tape_mut(2);
        buffer.tape_mut(7);
        let snapshot = buffer.snapshot().expect("tapes were created");
        let ids: Vec<u64> = snapshot.connections.iter().map(|tape| tape.id).collect();
        assert_eq!(
            ids,
            vec![7, 2],
            "tapes must be listed in the order the request first used each connection"
        );
    }
}
