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
#[derive(Debug, Default)]
pub struct DbBuffer {
    tapes: BTreeMap<u64, ConnectionTape>,
    bytes: usize,
}

impl DbBuffer {
    /// The tape for a connection, created on first use.
    pub fn tape_mut(&mut self, connection_id: u64) -> &mut ConnectionTape {
        self.tapes.entry(connection_id).or_insert_with(|| {
            let mut tape = ConnectionTape::default();
            tape.id = connection_id;
            tape
        })
    }

    /// Charge `bytes` against the capsule budget.
    ///
    /// Returns `false` once the budget is exhausted, at which point the caller
    /// must stop recording and mark the capsule truncated.
    pub fn charge(&mut self, bytes: usize, budget: usize) -> bool {
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

    /// Snapshot the tapes for serialization, in connection order.
    #[must_use]
    pub fn snapshot(&self) -> Option<CapsuleDb> {
        if self.tapes.is_empty() {
            return None;
        }
        Some(CapsuleDb {
            connections: self.tapes.values().cloned().collect(),
        })
    }
}

/// Everything one in-flight request has offered up for its capsule.
#[derive(Debug)]
pub struct CaptureScope {
    id: String,
    settings: Arc<CaptureSettings>,
    filter: Arc<ParameterFilter>,
    request: OnceLock<RawRequest>,
    clock: Mutex<Vec<DateTime<Utc>>>,
    db: Mutex<DbBuffer>,
    notes: Mutex<Vec<String>>,
    truncated: AtomicBool,
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
            clock: Mutex::new(Vec::new()),
            db: Mutex::new(DbBuffer::default()),
            notes: Mutex::new(Vec::new()),
            truncated: AtomicBool::new(false),
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
    #[must_use]
    pub fn raw_request(&self) -> Option<&RawRequest> {
        self.request.get()
    }

    /// Append a clock reading.
    pub fn record_clock(&self, reading: DateTime<Utc>) {
        // stub: recording lands in the GREEN step.
        let _ = reading;
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
    pub fn scope(&self) -> &Arc<CaptureScope> {
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

/// Removes a scope from the registry when the request's future is dropped,
/// including when it is dropped by a panic unwind.
struct RegistryGuard(String);

impl Drop for RegistryGuard {
    fn drop(&mut self) {
        deregister(&self.0);
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
        let _settings = Arc::clone(&self.settings);
        let _filter = Arc::clone(&self.filter);

        // stub: scope establishment lands in the GREEN step.
        Box::pin(async move { inner.call(req).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn db_buffer_snapshots_tapes_in_connection_order() {
        let mut buffer = DbBuffer::default();
        buffer.tape_mut(7);
        buffer.tape_mut(2);
        let snapshot = buffer.snapshot().expect("tapes were created");
        let ids: Vec<u64> = snapshot.connections.iter().map(|tape| tape.id).collect();
        assert_eq!(ids, vec![2, 7]);
    }
}
