//! The edge node: a reference edge target you can run (feature `node`).
//!
//! [`EdgeNode`] is an HTTP server. It puts a capsule in front of a remote
//! origin. It serves what the capsule serves. It sends each fallthrough to
//! the origin over HTTP and returns the origin's response. `autumn edge serve`
//! runs it. The app needs no extra code.
//!
//! [`ttfb::measure`] measures time to first byte at the node and at the
//! origin, and compares the bytes of each pair. `autumn edge ttfb` runs it.
//!
//! # Rules
//!
//! - **The origin is the authority.** Writes, declines and capsule errors go
//!   to the origin. The node keeps no state.
//! - **The node forwards requests exactly.** It does not follow redirects.
//!   It does not use `HTTP(S)_PROXY`. It removes hop-by-hop headers in both
//!   directions. It refuses a path with a dot segment (400).
//! - **The node is the first proxy.** It replaces the client's
//!   `x-forwarded-*` headers and removes `forwarded`. `host` is the
//!   origin's host.
//! - **Origin bodies stream.** The node does not hold a request or origin
//!   body in memory.
//! - **The capsule runs on a blocking thread**, at most one for each CPU at
//!   the same time. A slow capsule does not stop the async runtime.
//! - **An origin that does not connect or stops sending gives a 502.**

use std::convert::Infallible;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::body::{Body, HttpBody};
use axum::extract::ConnectInfo;
use http::header::{CONNECTION, CONTENT_TYPE, HOST};
use http::{HeaderMap, HeaderName, HeaderValue, Request, Response, StatusCode};
use tokio::sync::Semaphore;
use tower::Service;

use crate::conformance::{CORS_HEADERS, SECURITY_HEADERS};
use crate::gateway::{EdgeGateway, HOP_BY_HOP, Lane};

/// How long the node waits for a TCP connection to the origin.
pub const ORIGIN_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the node waits for the next bytes from the origin. A stalled
/// origin then gives a 502 (before the response head) or a cut body.
pub const ORIGIN_READ_TIMEOUT: Duration = Duration::from_secs(60);

const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");
const X_FORWARDED_HOST: HeaderName = HeaderName::from_static("x-forwarded-host");
const X_FORWARDED_PROTO: HeaderName = HeaderName::from_static("x-forwarded-proto");
const FORWARDED: HeaderName = HeaderName::from_static("forwarded");

/// A failure of the node or the probe. A capsule failure is not one of
/// these: it is a fallthrough.
#[derive(Debug)]
#[non_exhaustive]
pub enum NodeError {
    /// A URL, a path or a count is not valid.
    Config(String),
    /// A request did not complete: no connection, or the body broke.
    Request(String),
}

impl std::fmt::Display for NodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(message) => write!(f, "invalid configuration: {message}"),
            Self::Request(message) => write!(f, "request failed: {message}"),
        }
    }
}

impl std::error::Error for NodeError {}

/// The HTTP client the node and the probe use: no redirects, no proxy.
fn http_client() -> Result<reqwest::Client, NodeError> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(ORIGIN_CONNECT_TIMEOUT)
        .read_timeout(ORIGIN_READ_TIMEOUT)
        .build()
        .map_err(|err| NodeError::Config(format!("could not build the HTTP client: {err}")))
}

/// `http(s)://host[:port][/prefix]` without the trailing `/`. No query, no
/// fragment.
fn base_url(raw: &str) -> Result<String, NodeError> {
    let url = reqwest::Url::parse(raw)
        .map_err(|err| NodeError::Config(format!("`{raw}` is not a URL: {err}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(NodeError::Config(format!("`{raw}` must use http or https")));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(NodeError::Config(
            "the URL must not contain a user name or a password".into(),
        ));
    }
    if url.host_str().is_none() || url.query().is_some() || url.fragment().is_some() {
        return Err(NodeError::Config(format!(
            "`{raw}` must be scheme://host[:port][/path], without a query or a fragment"
        )));
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

/// The origin over HTTP, as a `tower::Service` for [`EdgeGateway`].
#[derive(Clone, Debug)]
pub struct HttpOrigin {
    client: reqwest::Client,
    base: String,
}

impl HttpOrigin {
    /// An origin at `base`, for example `https://origin.example.com`. A path
    /// in `base` prefixes every forwarded path.
    ///
    /// # Errors
    ///
    /// [`NodeError::Config`] when `base` is not an `http` or `https` URL, or
    /// has a query or a fragment.
    pub fn new(base: &str) -> Result<Self, NodeError> {
        Ok(Self {
            client: http_client()?,
            base: base_url(base)?,
        })
    }

    /// The base URL, without a trailing `/`.
    #[must_use]
    pub fn base(&self) -> &str {
        &self.base
    }
}

impl Service<Request<Body>> for HttpOrigin {
    type Response = Response<Body>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Response<Body>, Infallible>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: Request<Body>) -> Self::Future {
        let client = self.client.clone();
        let base = self.base.clone();
        Box::pin(async move { Ok(forward(&client, &base, request).await) })
    }
}

/// Send `request` to the origin and return its response, or a 502 (400 for an unsafe path).
async fn forward(client: &reqwest::Client, base: &str, request: Request<Body>) -> Response<Body> {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip());
    let (parts, body) = request.into_parts();
    let path = parts
        .uri
        .path_and_query()
        .map_or("/", http::uri::PathAndQuery::as_str);
    if !is_safe_path(path) {
        return bad_request();
    }
    let mut outgoing = client
        .request(parts.method, format!("{base}{path}"))
        .headers(forwarded_headers(&parts.headers, peer));
    // An empty body stays empty. A stream would make a GET chunked.
    if body.size_hint().exact() != Some(0) {
        outgoing = outgoing.body(reqwest::Body::wrap(SyncBody(Mutex::new(body))));
    }
    outgoing.send().await.map_or_else(
        |_| bad_gateway(),
        |answer| {
            let answer: Response<reqwest::Body> = answer.into();
            let (mut parts, body) = answer.into_parts();
            remove_hop_by_hop(&mut parts.headers);
            Response::from_parts(parts, Body::new(body))
        },
    )
}

/// A request body that is `Sync`, as `reqwest::Body::wrap` needs. Only the
/// body's own task polls it, so the lock is never contended.
struct SyncBody(Mutex<Body>);

impl HttpBody for SyncBody {
    type Data = axum::body::Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let body = self
            .get_mut()
            .0
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        Pin::new(body).poll_frame(cx)
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .size_hint()
    }
}

/// The request headers the origin gets. The node is the first proxy, so it
/// replaces the client's forwarded headers: `x-forwarded-for` is the peer,
/// `x-forwarded-host` is the request `host`, `x-forwarded-proto` is `http`.
/// It removes `forwarded`, `host` and the hop-by-hop headers.
fn forwarded_headers(incoming: &HeaderMap, peer: Option<IpAddr>) -> HeaderMap {
    let mut headers = incoming.clone();
    remove_hop_by_hop(&mut headers);
    for name in [
        X_FORWARDED_FOR,
        X_FORWARDED_HOST,
        X_FORWARDED_PROTO,
        FORWARDED,
    ] {
        headers.remove(name);
    }
    if let Some(host) = headers.remove(HOST) {
        headers.insert(X_FORWARDED_HOST, host);
    }
    headers.insert(X_FORWARDED_PROTO, HeaderValue::from_static("http"));
    if let Some(peer) = peer
        && let Ok(value) = HeaderValue::from_str(&peer.to_canonical().to_string())
    {
        headers.insert(X_FORWARDED_FOR, value);
    }
    headers
}

/// True when `path_and_query` starts with `/` and has no `.` or `..`
/// segment (also as `%2e`) and no `\`. An HTTP client resolves those
/// segments, so the origin would get another path than the capsule.
fn is_safe_path(path_and_query: &str) -> bool {
    let path = path_and_query
        .split_once('?')
        .map_or(path_and_query, |(path, _)| path);
    path.starts_with('/')
        && !path.contains('\\')
        && path.split('/').all(|segment| {
            let decoded = segment.to_ascii_lowercase().replace("%2e", ".");
            decoded != "." && decoded != ".."
        })
}

/// Remove the hop-by-hop headers, and each header that `connection` names.
fn remove_hop_by_hop(headers: &mut HeaderMap) {
    let named: Vec<HeaderName> = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|token| HeaderName::from_bytes(token.trim().as_bytes()).ok())
        .collect();
    for name in named {
        headers.remove(name);
    }
    for name in HOP_BY_HOP {
        headers.remove(*name);
    }
}

fn plain_response(status: StatusCode, body: &'static str) -> Response<Body> {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

fn bad_request() -> Response<Body> {
    plain_response(StatusCode::BAD_REQUEST, "Bad Request: unsafe path\n")
}

fn bad_gateway() -> Response<Body> {
    plain_response(
        StatusCode::BAD_GATEWAY,
        "Bad Gateway: the origin did not answer\n",
    )
}

/// One served request, for an access log.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AccessEntry {
    /// The request method.
    pub method: String,
    /// The request path, without the query: a query can hold a token.
    pub path: String,
    /// The response status.
    pub status: u16,
    /// The lane that answered. `None` when the node itself failed.
    pub lane: Option<Lane>,
    /// The time from the request to the response head.
    pub elapsed: Duration,
}

type AccessLog = Arc<dyn Fn(&AccessEntry) + Send + Sync>;

/// An [`EdgeGateway`] as an HTTP service.
///
/// The capsule runs on a blocking thread. At most [`EdgeNode::max_capsules`]
/// capsules run at the same time; other `GET`s wait. A write does not wait:
/// it goes to the origin without the capsule. A path with a `.` or `..`
/// segment, or a `\`, gets a 400.
pub struct EdgeNode<O> {
    gateway: EdgeGateway<O>,
    access_log: Option<AccessLog>,
    capsules: Arc<Semaphore>,
}

impl<O: Clone> Clone for EdgeNode<O> {
    fn clone(&self) -> Self {
        Self {
            gateway: self.gateway.clone(),
            access_log: self.access_log.clone(),
            capsules: Arc::clone(&self.capsules),
        }
    }
}

impl<O> std::fmt::Debug for EdgeNode<O> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EdgeNode")
            .field("gateway", &self.gateway)
            .field("access_log", &self.access_log.is_some())
            .finish()
    }
}

impl<O> EdgeNode<O> {
    /// A node that serves through `gateway`. It runs as many capsules at
    /// the same time as the machine has CPUs.
    pub fn new(gateway: EdgeGateway<O>) -> Self {
        let cpus = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
        Self {
            gateway,
            access_log: None,
            capsules: Arc::new(Semaphore::new(cpus)),
        }
    }

    /// Run at most `max` capsules at the same time (at least 1).
    #[must_use]
    pub fn with_max_capsules(mut self, max: usize) -> Self {
        self.capsules = Arc::new(Semaphore::new(max.max(1)));
        self
    }

    /// How many capsules can run at the same time now.
    #[must_use]
    pub fn max_capsules(&self) -> usize {
        self.capsules.available_permits()
    }

    /// Call `log` once for each request, after the response head is ready.
    /// Use it to count the lanes: it shows how much the edge serves.
    #[must_use]
    pub fn with_access_log(mut self, log: impl Fn(&AccessEntry) + Send + Sync + 'static) -> Self {
        self.access_log = Some(Arc::new(log));
        self
    }
}

impl<O> Service<Request<Body>> for EdgeNode<O>
where
    O: Service<Request<Body>, Response = Response<Body>, Error = Infallible>
        + Clone
        + Send
        + Sync
        + 'static,
    O::Future: Send,
{
    type Response = Response<Body>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Response<Body>, Infallible>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: Request<Body>) -> Self::Future {
        let gateway = self.gateway.clone();
        let access_log = self.access_log.clone();
        let capsules = Arc::clone(&self.capsules);
        let started = Instant::now();
        let method = request.method().as_str().to_owned();
        let path = request.uri().path().to_owned();
        let safe = request
            .uri()
            .path_and_query()
            .is_some_and(|target| is_safe_path(target.as_str()));
        let runs_capsule = matches!(method.as_str(), "GET" | "HEAD");
        Box::pin(async move {
            let response = if !safe {
                bad_request()
            } else if runs_capsule {
                // A closed semaphore cannot happen: the node never closes it.
                let permit = capsules.acquire_owned().await.ok();
                let answer = tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    gateway.handle(request)
                })
                .await;
                match answer {
                    Ok(origin_or_edge) => origin_or_edge.await,
                    Err(_) => plain_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Internal Server Error: the edge node failed\n",
                    ),
                }
            } else {
                gateway.handle(request).await
            };
            if let Some(log) = access_log {
                log(&AccessEntry {
                    method,
                    path,
                    status: response.status().as_u16(),
                    lane: response.extensions().get::<Lane>().copied(),
                    elapsed: started.elapsed(),
                });
            }
            Ok(response)
        })
    }
}

/// Serve `service` on `listener` until `shutdown` completes.
///
/// The service gets the peer address as `ConnectInfo<SocketAddr>`, so
/// [`HttpOrigin`] can set `x-forwarded-for`.
///
/// # Errors
///
/// An I/O error from the listener.
pub async fn serve<S>(
    listener: tokio::net::TcpListener,
    service: S,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()>
where
    S: Service<Request<Body>, Response = Response<Body>, Error = Infallible>
        + Clone
        + Send
        + Sync
        + 'static,
    S::Future: Send + 'static,
{
    let router = axum::Router::new().fallback_service(service);
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await
}

/// The origin's static middleware headers, read from two `GET`s of `path`.
///
/// It copies each header in [`SECURITY_HEADERS`], [`CORS_HEADERS`] and
/// `content-security-policy` that both responses send with the same values.
/// A header that changes (a CSP nonce, a CORS policy per request) is not
/// copied. Give the result to [`EdgeGateway::with_response_headers`].
///
/// # Errors
///
/// [`NodeError::Config`] for a bad URL. [`NodeError::Request`] when the
/// origin does not answer.
pub async fn origin_static_headers(
    origin: &str,
    path: &str,
) -> Result<Vec<(HeaderName, HeaderValue)>, NodeError> {
    if !path.starts_with('/') {
        return Err(NodeError::Config(format!(
            "path `{path}` must start with `/`"
        )));
    }
    let url = format!("{}{path}", base_url(origin)?);
    let client = http_client()?;
    let get = || async {
        client
            .get(&url)
            .send()
            .await
            .map(|answer| answer.headers().clone())
            .map_err(|err| NodeError::Request(format!("GET {url}: {err}")))
    };
    let first = get().await?;
    let second = get().await?;

    let names = SECURITY_HEADERS
        .iter()
        .chain(CORS_HEADERS)
        .chain(&["content-security-policy"]);
    let mut headers = Vec::new();
    for name in names {
        let values: Vec<&HeaderValue> = first.get_all(*name).iter().collect();
        let again: Vec<&HeaderValue> = second.get_all(*name).iter().collect();
        if values.is_empty() || values != again {
            continue;
        }
        let name = HeaderName::from_static(name);
        headers.extend(
            values
                .into_iter()
                .map(|value| (name.clone(), value.clone())),
        );
    }
    Ok(headers)
}

/// Time to first byte, edge against origin (the issue #1790 success metric).
///
/// [`measure`](ttfb::measure) sends the same `GET`s to the edge node and to
/// the origin. For each path, it changes which side goes first on each
/// round. It records the time to the response head, then reads the body. It
/// compares the pair with [`conformance::compare`](crate::conformance::compare),
/// without the hop-by-hop headers. A pair that is not equal is a divergence.
pub mod ttfb {
    use std::time::{Duration, Instant};

    use super::{NodeError, base_url, http_client};
    use crate::conformance::{Verdict, compare};
    use crate::wire::EdgeResponse;

    /// What to measure.
    #[derive(Clone, Debug)]
    pub struct Probe {
        /// The edge node base URL.
        pub edge: String,
        /// The origin base URL.
        pub origin: String,
        /// The paths to request, each with a leading `/`. A query is allowed.
        pub paths: Vec<String>,
        /// How many times to request each path.
        pub rounds: usize,
    }

    /// TTFB samples of one side.
    #[derive(Clone, Debug, Default)]
    pub struct Summary {
        /// One sample per request, in request order.
        pub samples: Vec<Duration>,
    }

    impl Summary {
        /// The `percent` percentile, nearest rank. Zero with no samples.
        #[must_use]
        pub fn percentile(&self, percent: usize) -> Duration {
            let mut sorted = self.samples.clone();
            sorted.sort_unstable();
            let rank = sorted.len().saturating_mul(percent.min(100)).div_ceil(100);
            sorted
                .get(rank.saturating_sub(1))
                .copied()
                .unwrap_or_default()
        }

        /// The median.
        #[must_use]
        pub fn median(&self) -> Duration {
            self.percentile(50)
        }

        /// The 90th percentile.
        #[must_use]
        pub fn p90(&self) -> Duration {
            self.percentile(90)
        }
    }

    /// The result of [`measure`].
    #[derive(Clone, Debug, Default)]
    pub struct Report {
        /// TTFB at the edge node.
        pub edge: Summary,
        /// TTFB at the origin.
        pub origin: Summary,
        /// One line per pair that is not equal.
        pub divergences: Vec<String>,
    }

    impl Report {
        /// How much lower the edge median is than the origin median, in
        /// percent. Zero when the origin median is zero.
        #[must_use]
        pub fn reduction_percent(&self) -> f64 {
            let origin = self.origin.median().as_secs_f64();
            if origin <= 0.0 {
                return 0.0;
            }
            (1.0 - self.edge.median().as_secs_f64() / origin) * 100.0
        }

        /// True when both sides have samples, no pair diverged, and the
        /// reduction is at least `min_reduction_percent`.
        #[must_use]
        pub fn passes(&self, min_reduction_percent: f64) -> bool {
            !self.edge.samples.is_empty()
                && !self.origin.samples.is_empty()
                && self.divergences.is_empty()
                && self.reduction_percent() >= min_reduction_percent
        }
    }

    /// Run the probe.
    ///
    /// # Errors
    ///
    /// [`NodeError::Config`] for a bad URL, an empty path list, a path
    /// without a leading `/`, or zero rounds. [`NodeError::Request`] when a
    /// side does not answer.
    pub async fn measure(probe: &Probe) -> Result<Report, NodeError> {
        let edge = base_url(&probe.edge)?;
        let origin = base_url(&probe.origin)?;
        if probe.paths.is_empty() || probe.rounds == 0 {
            return Err(NodeError::Config(
                "give at least one path and at least one round".into(),
            ));
        }
        if let Some(path) = probe.paths.iter().find(|path| !path.starts_with('/')) {
            return Err(NodeError::Config(format!(
                "path `{path}` must start with `/`"
            )));
        }
        let client = http_client()?;

        // Open a connection to each side first. The first request pays for
        // the TCP handshake; that is not what the probe measures.
        for base in [&edge, &origin] {
            if let Some(path) = probe.paths.first() {
                fetch(&client, base, path).await?;
            }
        }

        let mut report = Report::default();
        for round in 0..probe.rounds {
            for (index, path) in probe.paths.iter().enumerate() {
                // Each path changes its order on each round.
                let edge_first = round.wrapping_add(index) % 2 == 0;
                let (edge_answer, origin_answer) = if edge_first {
                    let e = fetch(&client, &edge, path).await?;
                    (e, fetch(&client, &origin, path).await?)
                } else {
                    let o = fetch(&client, &origin, path).await?;
                    (fetch(&client, &edge, path).await?, o)
                };
                report.edge.samples.push(edge_answer.0);
                report.origin.samples.push(origin_answer.0);
                if let Verdict::Diverged { detail } = compare(&origin_answer.1, &edge_answer.1) {
                    report.divergences.push(format!("GET {path}: {detail}"));
                }
            }
        }
        Ok(report)
    }

    /// `GET base+path`: the time to the response head, and the response.
    async fn fetch(
        client: &reqwest::Client,
        base: &str,
        path: &str,
    ) -> Result<(Duration, EdgeResponse), NodeError> {
        let url = format!("{base}{path}");
        let failed = |err: reqwest::Error| NodeError::Request(format!("GET {url}: {err}"));
        let started = Instant::now();
        let answer = client.get(&url).send().await.map_err(failed)?;
        let ttfb = started.elapsed();
        let status = answer.status().as_u16();
        // Hop-by-hop headers belong to one connection, not to the response.
        let mut end_to_end = answer.headers().clone();
        super::remove_hop_by_hop(&mut end_to_end);
        let headers = end_to_end
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect();
        let body = answer.bytes().await.map_err(failed)?.to_vec();
        Ok((
            ttfb,
            EdgeResponse {
                status,
                headers,
                body,
            },
        ))
    }
}
