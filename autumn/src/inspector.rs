//! Dev-mode request inspector with N+1 query detection.
//!
//! When running in the `dev` profile, autumn automatically mounts a request
//! inspector UI at `/_autumn/inspect` (configurable via `[dev]
//! inspector_path`).
//!
//! The inspector records the last N requests (default 100) in a bounded
//! ring buffer and flags any request that issued ≥ M structurally identical
//! SQL statements (default M = 5) as an N+1 candidate.
//!
//! # Production safety
//!
//! The inspector is `dev`-only by hard contract: the route returns `404`
//! in `prod` and `test` profiles regardless of configuration. The
//! instrumentation overhead is also bounded — when disabled (non-dev
//! profile), the `InspectorLayer` is never mounted and the path is
//! completely absent.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};

// ── Core data types ───────────────────────────────────────────────────────────

/// A single SQL query recorded during a request.
#[derive(Debug, Clone)]
pub struct QueryRecord {
    /// The SQL text (may contain `$N` placeholders).
    pub sql: String,
    /// Bound parameter values (verbatim in dev; PII filtering deferred to #697).
    pub params: Vec<String>,
    /// Execution time in milliseconds.
    pub elapsed_ms: u64,
    /// Call site that issued this query, e.g. `"src/posts.rs:42"`.
    pub location: String,
}

/// Details of an N+1 query warning.
#[derive(Debug, Clone)]
pub struct NPlusOneWarning {
    /// The normalised SQL template that was repeated.
    pub sql_template: String,
    /// How many times it appeared in this request.
    pub count: usize,
}

/// A complete record for one HTTP request.
#[derive(Debug, Clone)]
pub struct RequestRecord {
    /// Monotonically increasing ID assigned by [`InspectorBuffer::push`].
    pub id: u64,
    /// HTTP method (e.g. `"GET"`).
    pub method: String,
    /// Request path (e.g. `"/posts"`).
    pub path: String,
    /// Matched route pattern (e.g. `"/posts/{id}"`), set by Axum's router.
    /// `None` when the route was not matched (404) or `MatchedPath` was unavailable.
    pub route: Option<String>,
    /// HTTP status code.
    pub status: u16,
    /// Total wall time for the request, in milliseconds.
    pub elapsed_ms: u64,
    /// Value of the `Content-Type` response header, if present.
    pub content_type: Option<String>,
    /// Value of the `Content-Length` response header, if present.
    pub content_length: Option<u64>,
    /// Session identifier parsed from the session cookie, if present.
    pub session_id: Option<String>,
    /// SQL queries issued during this request (via [`RequestInspector`]).
    pub queries: Vec<QueryRecord>,
    /// Set when an N+1 pattern was detected, otherwise `None`.
    pub n_plus_one: Option<NPlusOneWarning>,
    /// Unix timestamp (seconds) when the record was added to the buffer.
    pub recorded_at: u64,
}

impl RequestRecord {
    /// Number of SQL queries issued during this request.
    #[must_use]
    pub const fn query_count(&self) -> usize {
        self.queries.len()
    }

    /// A `curl` one-liner that reproduces the method + path of this request.
    #[must_use]
    pub fn curl_snippet(&self) -> String {
        format!(
            "curl -X {} 'http://localhost:3000{}'",
            self.method, self.path
        )
    }
}

// ── Ring buffer ───────────────────────────────────────────────────────────────

/// Thread-safe ring buffer that holds the last N [`RequestRecord`]s.
///
/// Records are stored newest-first. When the buffer is full the oldest
/// record is dropped to make room for the new one.
#[derive(Debug, Clone)]
pub struct InspectorBuffer {
    inner: Arc<Mutex<InspectorInner>>,
}

#[derive(Debug)]
struct InspectorInner {
    records: VecDeque<RequestRecord>,
    capacity: usize,
    next_id: u64,
}

impl InspectorBuffer {
    /// Create a new buffer with the given capacity.
    ///
    /// A capacity of `0` disables recording (all pushes are no-ops).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(InspectorInner {
                records: VecDeque::with_capacity(capacity.min(512)),
                capacity,
                next_id: 1,
            })),
        }
    }

    /// Push a new record. If at capacity the oldest record is dropped first.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn push(&self, mut record: RequestRecord) {
        let mut g = self.inner.lock().expect("inspector buffer lock poisoned");
        if g.capacity == 0 {
            return;
        }
        record.id = g.next_id;
        g.next_id += 1;
        if g.records.len() >= g.capacity {
            g.records.pop_back();
        }
        g.records.push_front(record);
    }

    /// Snapshot of all records, newest first.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    #[must_use]
    pub fn snapshot(&self) -> Vec<RequestRecord> {
        self.inner
            .lock()
            .expect("inspector buffer lock poisoned")
            .records
            .iter()
            .cloned()
            .collect()
    }

    /// Look up a single record by its ID.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    #[must_use]
    pub fn get(&self, id: u64) -> Option<RequestRecord> {
        self.inner
            .lock()
            .expect("inspector buffer lock poisoned")
            .records
            .iter()
            .find(|r| r.id == id)
            .cloned()
    }

    /// The configured capacity.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.inner
            .lock()
            .expect("inspector buffer lock poisoned")
            .capacity
    }
}

// ── N+1 detector ─────────────────────────────────────────────────────────────

/// Default N+1 detection threshold: the minimum number of structurally
/// identical SQL statements in a single request before an N+1 warning fires.
///
/// Single source of truth shared by `dev.inspector_n_plus_one_threshold`'s
/// config default and the `Default` impl of `crate::test::TestResponse`. A
/// threshold of `0` disables detection, so directly-constructed responses must
/// inherit this non-zero default rather than a zero-filled one.
pub(crate) const DEFAULT_N_PLUS_ONE_THRESHOLD: usize = 5;

/// Examine a query list and return a warning if any SQL template was issued
/// ≥ `threshold` times. Returns `None` when below threshold, or when
/// `threshold == 0`, or when `queries` is empty.
///
/// "Structurally identical" means the SQL is identical after collapsing
/// all whitespace and converting to lower-case. This catches formatting
/// differences between call sites while still allowing different predicates
/// to be treated as different queries.
#[must_use]
pub fn detect_n_plus_one(queries: &[QueryRecord], threshold: usize) -> Option<NPlusOneWarning> {
    if threshold == 0 || queries.is_empty() {
        return None;
    }
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for q in queries {
        *counts.entry(normalize_sql(&q.sql)).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .filter(|(_, c)| *c >= threshold)
        .max_by_key(|(_, c)| *c)
        .map(|(sql_template, count)| NPlusOneWarning {
            sql_template,
            count,
        })
}

/// Collapse whitespace and lower-case a SQL string for comparison.
fn normalize_sql(sql: &str) -> String {
    sql.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

// ── Per-request query accumulator ─────────────────────────────────────────────

/// Shared, per-request container for SQL query records.
///
/// Injected into request extensions by [`InspectorLayer`] at the start of
/// every request (excluding the inspector's own routes). Handlers extract it
/// via [`RequestInspector`].
#[derive(Clone, Debug)]
pub(crate) struct RequestQueryList(Arc<Mutex<Vec<QueryRecord>>>);

impl Default for RequestQueryList {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(Vec::new())))
    }
}

impl RequestQueryList {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, record: QueryRecord) {
        self.0
            .lock()
            .expect("query list lock poisoned")
            .push(record);
    }

    pub fn snapshot(&self) -> Vec<QueryRecord> {
        self.0.lock().expect("query list lock poisoned").clone()
    }
}

// ── RequestInspector extractor ────────────────────────────────────────────────

/// Axum extractor that gives handlers access to the per-request query
/// accumulator.
///
/// Available when [`InspectorLayer`] is applied to the router (i.e. in the
/// `dev` profile). In integration tests this lets you assert on how many
/// SQL queries a handler issued without a UI round-trip.
///
/// # Example
///
/// ```rust,ignore
/// use autumn_web::inspector::{RequestInspector, QueryRecord};
///
/// #[get("/posts")]
/// async fn index(inspector: RequestInspector) -> &'static str {
///     inspector.record_query(QueryRecord {
///         sql: "SELECT * FROM posts".into(),
///         params: vec![],
///         elapsed_ms: 3,
///         location: "src/posts.rs:22".into(),
///     });
///     "ok"
/// }
/// ```
#[derive(Clone)]
pub struct RequestInspector {
    list: RequestQueryList,
}

impl RequestInspector {
    /// Append a SQL query record to this request's accumulated list.
    pub fn record_query(&self, record: QueryRecord) {
        self.list.push(record);
    }

    /// Number of queries recorded so far in this request.
    #[must_use]
    pub fn query_count(&self) -> usize {
        self.list.snapshot().len()
    }
}

impl<S: Send + Sync> FromRequestParts<S> for RequestInspector {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let list = parts
            .extensions
            .get::<RequestQueryList>()
            .cloned()
            .unwrap_or_default();
        Ok(Self { list })
    }
}

// ── Tower middleware ──────────────────────────────────────────────────────────

/// Tower [`Layer`] that records requests into [`InspectorBuffer`].
///
/// Apply this layer in `dev` profile to wrap your entire router. The layer
/// automatically excludes the inspector's own routes from recording to avoid
/// feedback loops.
///
/// [`Layer`]: tower::Layer
#[derive(Clone)]
pub struct InspectorLayer {
    buffer: InspectorBuffer,
    n_plus_one_threshold: usize,
    inspector_path_prefix: String,
    /// Name of the session cookie used to extract the session identifier.
    session_cookie_name: String,
}

impl InspectorLayer {
    /// Create a new layer.
    ///
    /// * `buffer` — shared ring buffer to write records into.
    /// * `n_plus_one_threshold` — minimum repetition count to trigger an N+1 warning.
    /// * `inspector_path_prefix` — path prefix of the inspector UI (excluded from recording).
    #[must_use]
    pub fn new(
        buffer: InspectorBuffer,
        n_plus_one_threshold: usize,
        inspector_path_prefix: String,
    ) -> Self {
        Self {
            buffer,
            n_plus_one_threshold,
            inspector_path_prefix,
            session_cookie_name: "autumn_session".to_owned(),
        }
    }

    /// Override the session cookie name used for session-ID extraction.
    #[must_use]
    pub fn with_session_cookie_name(mut self, name: impl Into<String>) -> Self {
        self.session_cookie_name = name.into();
        self
    }
}

impl<S> tower::Layer<S> for InspectorLayer {
    type Service = InspectorMiddleware<S>;

    fn layer(&self, inner: S) -> Self::Service {
        InspectorMiddleware {
            inner,
            buffer: self.buffer.clone(),
            n_plus_one_threshold: self.n_plus_one_threshold,
            inspector_path_prefix: self.inspector_path_prefix.clone(),
            session_cookie_name: self.session_cookie_name.clone(),
        }
    }
}

/// Tower service produced by [`InspectorLayer`].
#[derive(Clone)]
pub struct InspectorMiddleware<S> {
    inner: S,
    buffer: InspectorBuffer,
    n_plus_one_threshold: usize,
    inspector_path_prefix: String,
    session_cookie_name: String,
}

impl<S> tower::Service<axum::extract::Request> for InspectorMiddleware<S>
where
    S: tower::Service<axum::extract::Request, Response = Response> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response;
    type Error = S::Error;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: axum::extract::Request) -> Self::Future {
        let path = req
            .uri()
            .path_and_query()
            .map_or_else(|| req.uri().path().to_owned(), |pq| pq.as_str().to_owned());

        // Self-exclusion: don't record requests to the inspector's own subtree.
        // Use exact match or subtree prefix ("/prefix/") to avoid false-excluding
        // unrelated routes that share the same string prefix (e.g. "/_autumn/inspector").
        // `strip_prefix` + a boundary check gets the same semantics as
        // `format!("{prefix}/")` + `starts_with` without allocating a String
        // on every request.
        let is_inspector = path
            .strip_prefix(self.inspector_path_prefix.as_str())
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'));
        if is_inspector {
            let fut = self.inner.call(req);
            return Box::pin(fut);
        }

        let method = req.method().to_string();
        let buffer = self.buffer.clone();
        let threshold = self.n_plus_one_threshold;

        // Extract route pattern — available because Axum's Router::layer applies
        // after route dispatch, so MatchedPath is already set in extensions.
        let route = req
            .extensions()
            .get::<axum::extract::MatchedPath>()
            .map(|mp| mp.as_str().to_owned());

        // Extract session ID from the cookie header before the request is consumed.
        let session_id = extract_session_id(req.headers(), &self.session_cookie_name);

        // Inject per-request query list into extensions so handlers can call
        // RequestInspector::record_query(...).
        let query_list = RequestQueryList::new();
        req.extensions_mut().insert(query_list.clone());

        let start = Instant::now();
        let fut = self.inner.call(req);

        Box::pin(async move {
            let mut response = fut.await?;
            let elapsed_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

            let status = response.status().as_u16();
            let content_type = response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let content_length = response
                .headers()
                .get(header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse().ok());

            let queries = query_list.snapshot();
            let n_plus_one = detect_n_plus_one(&queries, threshold);
            let recorded_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());

            let record = RequestRecord {
                id: 0, // assigned by InspectorBuffer::push
                method,
                path,
                route,
                status,
                elapsed_ms,
                content_type,
                content_length,
                session_id,
                queries,
                n_plus_one,
                recorded_at,
            };
            buffer.push(record);

            // Propagate the per-request query list to response extensions so
            // the dev error overlay (ErrorPageContextLayer) can snapshot it.
            response.extensions_mut().insert(query_list);

            Ok(response)
        })
    }
}

/// Parse the session identifier from a `Cookie` header value.
///
/// The session cookie may be signed (`{id}.{hmac}`); we return only
/// the `id` portion (everything before the first `.`).
fn extract_session_id(headers: &axum::http::HeaderMap, cookie_name: &str) -> Option<String> {
    let cookie_header = headers.get(header::COOKIE)?.to_str().ok()?;
    for pair in cookie_header.split(';') {
        let pair = pair.trim();
        if let Some((name, value)) = pair.split_once('=')
            && name.trim() == cookie_name
        {
            let raw = value.trim();
            // Strip HMAC signature: `{session_id}.{hmac_hex}` → `{session_id}`
            let id = raw.split_once('.').map_or(raw, |(id, _)| id);
            if !id.is_empty() {
                return Some(id.to_owned());
            }
        }
    }
    None
}

// ── HTTP handlers ─────────────────────────────────────────────────────────────

/// Build the router for the inspector UI.
///
/// Mounts:
/// * `GET {path}` — request list (newest-first)
/// * `GET {path}/requests/{id}` — request detail
pub fn inspector_router<S>(buffer: InspectorBuffer, path: &str) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let detail_path = format!("{path}/requests/{{id}}");
    let buf_index = buffer.clone();
    let buf_detail = buffer;
    let path_for_index = path.to_owned();
    let path_for_detail = path.to_owned();

    axum::Router::new()
        .route(
            path,
            axum::routing::get(move || {
                let records = buf_index.snapshot();
                let p = path_for_index.clone();
                async move { Html(render_index(&records, &p)) }
            }),
        )
        .route(
            &detail_path,
            axum::routing::get(move |axum::extract::Path(id): axum::extract::Path<u64>| {
                let record = buf_detail.get(id);
                let p = path_for_detail.clone();
                async move {
                    record.map_or_else(
                        || StatusCode::NOT_FOUND.into_response(),
                        |r| Html(render_detail(&r, &p)).into_response(),
                    )
                }
            }),
        )
}

// ── HTML rendering ────────────────────────────────────────────────────────────

fn render_index(records: &[RequestRecord], inspector_path: &str) -> String {
    let mut body = String::new();
    body.push_str("<h1>Autumn Request Inspector</h1>");
    body.push_str("<p class=\"muted\">Newest requests first &middot; <a href=\"");
    body.push_str(&escape_html(inspector_path));
    body.push_str("\">Refresh</a></p>");

    if records.is_empty() {
        body.push_str(
            "<p class=\"empty\">No requests recorded yet. Make some requests then refresh.</p>",
        );
    } else {
        body.push_str("<table><thead><tr><th>Method</th><th>Path</th><th>Route</th><th>Status</th><th>Duration</th><th>Queries</th><th>N+1?</th></tr></thead><tbody>");
        for rec in records {
            let n1 = if rec.n_plus_one.is_some() {
                "⚠ N+1"
            } else {
                ""
            };
            let status_class = if rec.status >= 500 {
                "error"
            } else if rec.status >= 400 {
                "warn"
            } else {
                ""
            };
            let route_display = rec.route.as_deref().unwrap_or("—");
            body.push_str("<tr>");
            body.push_str("<td><code>");
            body.push_str(&escape_html(&rec.method));
            body.push_str("</code></td><td><a href=\"");
            body.push_str(&escape_html(inspector_path));
            body.push_str("/requests/");
            body.push_str(&rec.id.to_string());
            body.push_str("\">");
            body.push_str(&escape_html(&rec.path));
            body.push_str("</a></td><td class=\"muted\"><code>");
            body.push_str(&escape_html(route_display));
            body.push_str("</code></td><td class=\"");
            body.push_str(status_class);
            body.push_str("\">");
            body.push_str(&rec.status.to_string());
            body.push_str("</td><td>");
            body.push_str(&rec.elapsed_ms.to_string());
            body.push_str("ms</td><td>");
            body.push_str(&rec.queries.len().to_string());
            body.push_str("</td><td class=\"n1\">");
            body.push_str(&escape_html(n1));
            body.push_str("</td></tr>");
        }
        body.push_str("</tbody></table>");
    }

    render_layout("Autumn Inspector", inspector_path, &body)
}

fn render_detail(rec: &RequestRecord, inspector_path: &str) -> String {
    let mut body = String::new();
    body.push_str("<p><a href=\"");
    body.push_str(&escape_html(inspector_path));
    body.push_str("\">&larr; Back to request list</a></p>");
    body.push_str("<h1>");
    body.push_str(&escape_html(&rec.method));
    body.push(' ');
    body.push_str(&escape_html(&rec.path));
    body.push_str("</h1>");

    // Summary bar
    body.push_str("<dl class=\"summary\">");
    body.push_str("<dt>Status</dt><dd>");
    body.push_str(&rec.status.to_string());
    body.push_str("</dd><dt>Duration</dt><dd>");
    body.push_str(&rec.elapsed_ms.to_string());
    body.push_str("ms</dd><dt>Queries</dt><dd>");
    body.push_str(&rec.queries.len().to_string());
    if let Some(route) = &rec.route {
        body.push_str("</dd><dt>Route</dt><dd><code>");
        body.push_str(&escape_html(route));
        body.push_str("</code>");
    }
    if let Some(sid) = &rec.session_id {
        body.push_str("</dd><dt>Session&nbsp;ID</dt><dd><code>");
        // Show only first 8 chars to avoid exposing the full token in the UI
        let truncated = if sid.len() > 8 { &sid[..8] } else { sid };
        body.push_str(&escape_html(truncated));
        body.push_str("…</code>");
    }
    if let Some(ct) = &rec.content_type {
        body.push_str("</dd><dt>Content-Type</dt><dd>");
        body.push_str(&escape_html(ct));
    }
    if let Some(cl) = &rec.content_length {
        body.push_str("</dd><dt>Content-Length</dt><dd>");
        body.push_str(&cl.to_string());
        body.push_str(" bytes");
    }
    body.push_str("</dd></dl>");

    // N+1 warning banner
    if let Some(w) = &rec.n_plus_one {
        body.push_str(
            "<div class=\"n1-banner\"><strong>&#9888; N+1 detected:</strong> query issued ",
        );
        body.push_str(&w.count.to_string());
        body.push_str(" times — <code>");
        body.push_str(&escape_html(&w.sql_template));
        body.push_str("</code></div>");
    }

    // Query list
    if rec.queries.is_empty() {
        body.push_str("<p class=\"muted\">No SQL queries recorded for this request.</p>");
        body.push_str("<p class=\"muted\">Use the <code>RequestInspector</code> extractor in your handler to record queries.</p>");
    } else {
        body.push_str("<h2>SQL queries (");
        body.push_str(&rec.queries.len().to_string());
        body.push_str(")</h2><table><thead><tr><th>#</th><th>SQL</th><th>Duration</th><th>Location</th></tr></thead><tbody>");
        for (i, q) in rec.queries.iter().enumerate() {
            body.push_str("<tr><td>");
            body.push_str(&(i + 1).to_string());
            body.push_str("</td><td><pre>");
            body.push_str(&escape_html(&q.sql));
            if !q.params.is_empty() {
                body.push_str("\n-- params: [");
                body.push_str(&escape_html(&q.params.join(", ")));
                body.push(']');
            }
            body.push_str("</pre></td><td>");
            body.push_str(&q.elapsed_ms.to_string());
            body.push_str("ms</td><td><code>");
            body.push_str(&escape_html(&q.location));
            body.push_str("</code></td></tr>");
        }
        body.push_str("</tbody></table>");
    }

    // curl snippet
    body.push_str("<details><summary>Reproduce this request</summary><pre>");
    body.push_str(&escape_html(&rec.curl_snippet()));
    body.push_str("</pre></details>");

    render_layout(
        &format!("{} {}", rec.method, rec.path),
        inspector_path,
        &body,
    )
}

fn render_layout(title: &str, _inspector_path: &str, body: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title><style>{css}</style></head><body>{body}</body></html>",
        title = escape_html(title),
        css = INSPECTOR_CSS,
        body = body,
    )
}

fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

const INSPECTOR_CSS: &str = r"
body{margin:0;padding:24px;font-family:system-ui,-apple-system,sans-serif;color:#1f2933;background:#f6f8fa;font-size:14px}
h1{margin:0 0 8px;font-size:22px}
h2{margin:20px 0 8px;font-size:16px}
a{color:#0b63ce;text-decoration:none}
a:hover{text-decoration:underline}
table{width:100%;border-collapse:collapse;background:#fff;border:1px solid #d9e2ec;margin:12px 0}
th,td{padding:8px 10px;border-bottom:1px solid #e5eaf0;text-align:left;vertical-align:top}
th{background:#edf2f7;font-weight:600}
code,pre{font-family:ui-monospace,SFMono-Regular,Consolas,monospace}
pre{margin:0;white-space:pre-wrap;background:#111827;color:#f8fafc;padding:8px;overflow:auto;border-radius:4px}
.empty,.muted{color:#52616f}
.error{color:#c0392b;font-weight:600}
.warn{color:#b7791f}
.n1{color:#c05621;font-weight:600}
.n1-banner{margin:12px 0;padding:10px 14px;background:#fff7ed;border:1px solid #fed7aa;border-radius:4px;color:#9a3412}
dl.summary{display:flex;flex-wrap:wrap;gap:4px 20px;margin:10px 0 16px}
dl.summary dt{font-weight:600;margin-right:4px}
dl.summary dd{margin:0}
details{margin:12px 0;padding:10px 12px;background:#fff;border:1px solid #d9e2ec;border-radius:4px}
summary{cursor:pointer;font-weight:600}
";

// ── Public path constant ──────────────────────────────────────────────────────

/// Default path for the inspector UI.
pub const INSPECTOR_DEFAULT_PATH: &str = "/_autumn/inspect";

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn make_record(method: &str, path: &str, status: u16) -> RequestRecord {
        RequestRecord {
            id: 0,
            method: method.to_owned(),
            path: path.to_owned(),
            route: None,
            status,
            elapsed_ms: 10,
            content_type: None,
            content_length: None,
            session_id: None,
            queries: vec![],
            n_plus_one: None,
            recorded_at: 0,
        }
    }

    fn make_query(sql: &str) -> QueryRecord {
        QueryRecord {
            sql: sql.to_owned(),
            params: vec![],
            elapsed_ms: 1,
            location: "test:1".to_owned(),
        }
    }

    // Buffer tests

    #[test]
    fn buffer_starts_empty() {
        assert_eq!(InspectorBuffer::new(10).snapshot().len(), 0);
    }

    #[test]
    fn buffer_capacity_zero_drops_all() {
        let buf = InspectorBuffer::new(0);
        buf.push(make_record("GET", "/x", 200));
        assert_eq!(buf.snapshot().len(), 0);
    }

    #[test]
    fn buffer_newest_first_ordering() {
        let buf = InspectorBuffer::new(5);
        buf.push(make_record("GET", "/old", 200));
        buf.push(make_record("GET", "/new", 200));
        let snap = buf.snapshot();
        assert_eq!(snap[0].path, "/new");
        assert_eq!(snap[1].path, "/old");
    }

    #[test]
    fn buffer_drops_oldest_at_capacity() {
        let buf = InspectorBuffer::new(2);
        buf.push(make_record("GET", "/oldest", 200));
        buf.push(make_record("GET", "/middle", 200));
        buf.push(make_record("GET", "/newest", 200));
        let snap = buf.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].path, "/newest");
        assert_eq!(snap[1].path, "/middle");
    }

    #[test]
    fn buffer_assigns_monotonic_ids() {
        let buf = InspectorBuffer::new(10);
        buf.push(make_record("GET", "/a", 200));
        buf.push(make_record("GET", "/b", 200));
        let snap = buf.snapshot();
        // newest is snap[0], so its id is higher
        assert!(snap[0].id > snap[1].id);
    }

    #[test]
    fn buffer_get_by_id() {
        let buf = InspectorBuffer::new(10);
        buf.push(make_record("GET", "/a", 200));
        let id = buf.snapshot()[0].id;
        let rec = buf.get(id).expect("should find by id");
        assert_eq!(rec.path, "/a");
    }

    // N+1 detection tests

    #[test]
    fn detect_empty_returns_none() {
        assert!(detect_n_plus_one(&[], 5).is_none());
    }

    #[test]
    fn detect_zero_threshold_returns_none() {
        let q = vec![make_query("SELECT * FROM t"); 10];
        assert!(detect_n_plus_one(&q, 0).is_none());
    }

    #[test]
    fn detect_fires_at_threshold() {
        let q = vec![make_query("SELECT * FROM users WHERE id = $1"); 5];
        let w = detect_n_plus_one(&q, 5).expect("should fire");
        assert_eq!(w.count, 5);
    }

    #[test]
    fn detect_does_not_fire_below_threshold() {
        let q = vec![make_query("SELECT * FROM users WHERE id = $1"); 4];
        assert!(detect_n_plus_one(&q, 5).is_none());
    }

    #[test]
    fn detect_normalizes_whitespace() {
        let queries = vec![
            make_query("SELECT   *   FROM users WHERE id = $1"),
            make_query("SELECT * FROM users WHERE id = $1"),
            make_query("SELECT *  FROM users WHERE  id = $1"),
            make_query("SELECT * FROM users WHERE id = $1"),
            make_query("SELECT * FROM users WHERE id = $1"),
        ];
        assert!(
            detect_n_plus_one(&queries, 5).is_some(),
            "whitespace-normalized queries should be treated as identical"
        );
    }

    #[test]
    fn detect_picks_worst_offender() {
        let mut queries = vec![make_query("SELECT * FROM users WHERE id = $1"); 3];
        queries.extend(vec![make_query("SELECT * FROM posts WHERE id = $1"); 7]);
        let w = detect_n_plus_one(&queries, 3).expect("should fire");
        assert_eq!(w.count, 7);
    }

    // normalize_sql tests

    #[test]
    fn normalize_collapses_whitespace_and_lowercases() {
        assert_eq!(
            normalize_sql("SELECT  *  FROM  Users"),
            "select * from users"
        );
    }

    // HTML escaping

    #[test]
    fn escape_html_escapes_special_chars() {
        assert_eq!(
            escape_html("<script>&\"'"),
            "&lt;script&gt;&amp;&quot;&#39;"
        );
    }

    // Middleware tests

    #[tokio::test]
    async fn middleware_records_request() {
        let buf = InspectorBuffer::new(10);
        let layer = InspectorLayer::new(buf.clone(), 5, "/_autumn/inspect".to_owned());
        let app = axum::Router::new()
            .route("/hi", axum::routing::get(|| async { "hi" }))
            .layer(layer);
        let req = Request::builder().uri("/hi").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        let snap = buf.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].method, "GET");
        assert_eq!(snap[0].path, "/hi");
        assert_eq!(snap[0].status, 200);
    }

    #[tokio::test]
    async fn middleware_self_excludes_inspector_routes() {
        let buf = InspectorBuffer::new(10);
        let layer = InspectorLayer::new(buf.clone(), 5, "/_autumn/inspect".to_owned());
        let app = axum::Router::new()
            .route("/_autumn/inspect", axum::routing::get(|| async { "ui" }))
            .layer(layer);
        let req = Request::builder()
            .uri("/_autumn/inspect")
            .body(Body::empty())
            .unwrap();
        let _ = app.oneshot(req).await.unwrap();
        assert_eq!(buf.snapshot().len(), 0);
    }

    #[tokio::test]
    async fn middleware_collects_queries_via_extractor() {
        let buf = InspectorBuffer::new(10);
        let layer = InspectorLayer::new(buf.clone(), 5, "/_autumn/inspect".to_owned());
        let app = axum::Router::new()
            .route(
                "/handler",
                axum::routing::get(|insp: RequestInspector| async move {
                    insp.record_query(QueryRecord {
                        sql: "SELECT 1".to_owned(),
                        params: vec![],
                        elapsed_ms: 1,
                        location: "test:1".to_owned(),
                    });
                    "ok"
                }),
            )
            .layer(layer);
        let req = Request::builder()
            .uri("/handler")
            .body(Body::empty())
            .unwrap();
        let _ = app.oneshot(req).await.unwrap();
        let snap = buf.snapshot();
        assert_eq!(snap[0].query_count(), 1);
        assert_eq!(snap[0].queries[0].sql, "SELECT 1");
    }

    #[tokio::test]
    async fn middleware_captures_matched_route_pattern() {
        let buf = InspectorBuffer::new(10);
        let layer = InspectorLayer::new(buf.clone(), 5, "/_autumn/inspect".to_owned());
        let app = axum::Router::new()
            .route("/items/{id}", axum::routing::get(|| async { "item" }))
            .layer(layer);
        let req = Request::builder()
            .uri("/items/99")
            .body(Body::empty())
            .unwrap();
        let _ = app.oneshot(req).await.unwrap();
        let snap = buf.snapshot();
        assert_eq!(snap[0].path, "/items/99");
        assert_eq!(snap[0].route.as_deref(), Some("/items/{id}"));
    }

    #[test]
    fn extract_session_id_finds_named_cookie() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            header::COOKIE,
            axum::http::HeaderValue::from_static("other=x; my_sess=abc123; foo=bar"),
        );
        let id = extract_session_id(&headers, "my_sess");
        assert_eq!(id.as_deref(), Some("abc123"));
    }

    #[test]
    fn extract_session_id_strips_hmac_suffix() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            header::COOKIE,
            axum::http::HeaderValue::from_static("sess=sessionid.hmacdata"),
        );
        let id = extract_session_id(&headers, "sess");
        assert_eq!(id.as_deref(), Some("sessionid"));
    }

    #[test]
    fn extract_session_id_returns_none_when_absent() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            header::COOKIE,
            axum::http::HeaderValue::from_static("other=val"),
        );
        let id = extract_session_id(&headers, "my_sess");
        assert!(id.is_none());
    }

    #[test]
    fn extract_session_id_returns_none_with_no_cookie_header() {
        let headers = axum::http::HeaderMap::new();
        let id = extract_session_id(&headers, "sess");
        assert!(id.is_none());
    }
}
