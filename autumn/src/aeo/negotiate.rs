//! The AEO response layer.
//!
//! - `Accept: text/markdown` on a `GET` for an HTML page returns Markdown.
//! - Every negotiable HTML response carries `Vary: Accept`, so a cache keeps
//!   the two forms apart.
//! - The homepage (`/`) carries the agent `Link` header.
//!
//! The layer sits inside response compression, so it reads plain bytes.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::{Body, Bytes};
use axum::http::header::{
    CONTENT_DISPOSITION, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG,
    LAST_MODIFIED, LINK, VARY,
};
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode};
use axum::response::Response;
use futures::StreamExt as _;
use http_body_util::BodyExt as _;

/// Settings the layer needs, built once per router.
#[derive(Debug, Clone, Default)]
pub struct NegotiateConfig {
    /// Convert HTML to Markdown on request.
    pub markdown: bool,
    /// Largest HTML body to convert.
    pub max_bytes: usize,
    /// `Link` header for `/`.
    pub home_link: Option<HeaderValue>,
    /// `Content-Signal` header for Markdown responses.
    pub content_signal: Option<HeaderValue>,
    /// `WWW-Authenticate` value for a `401` (RFC 9728 `resource_metadata`).
    pub resource_metadata: Option<HeaderValue>,
}

/// Tower layer for [`NegotiateConfig`].
#[derive(Debug, Clone)]
pub struct NegotiateLayer {
    config: Arc<NegotiateConfig>,
}

impl NegotiateLayer {
    /// Build the layer.
    #[must_use]
    pub fn new(config: NegotiateConfig) -> Self {
        Self {
            config: Arc::new(config),
        }
    }
}

impl<S> tower::Layer<S> for NegotiateLayer {
    type Service = NegotiateService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        NegotiateService {
            inner,
            config: Arc::clone(&self.config),
        }
    }
}

/// Service made by [`NegotiateLayer`].
#[derive(Debug, Clone)]
pub struct NegotiateService<S> {
    inner: S,
    config: Arc<NegotiateConfig>,
}

impl<S> tower::Service<Request<Body>> for NegotiateService<S>
where
    S: tower::Service<Request<Body>, Response = Response>,
    S::Future: Send + 'static,
{
    type Response = Response;
    type Error = S::Error;
    type Future = NegotiateFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let is_get = req.method() == Method::GET;
        let readable = is_get || req.method() == Method::HEAD;
        let flags = Flags {
            is_home: readable && req.uri().path() == "/",
            readable,
            wants_markdown: self.config.markdown && readable && prefers_markdown(req.headers()),
            is_head: !is_get && readable,
        };
        // The future holds only the inner future. It does not clone the
        // service. It boxes only a page that it converts.
        NegotiateFuture {
            inner: self.inner.call(req),
            config: Arc::clone(&self.config),
            flags,
            convert: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
#[allow(clippy::struct_excessive_bools)] // request facts, read once
struct Flags {
    is_home: bool,
    readable: bool,
    wants_markdown: bool,
    is_head: bool,
}

type ConvertFuture = Pin<Box<dyn Future<Output = Response> + Send>>;

pin_project_lite::pin_project! {
    /// Future of [`NegotiateService`].
    pub struct NegotiateFuture<F> {
        #[pin]
        inner: F,
        config: Arc<NegotiateConfig>,
        flags: Flags,
        convert: Option<ConvertFuture>,
    }
}

impl<F, E> Future for NegotiateFuture<F>
where
    F: Future<Output = Result<Response, E>>,
{
    type Output = Result<Response, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        if let Some(convert) = this.convert.as_mut() {
            return convert.as_mut().poll(cx).map(Ok);
        }
        let mut res = std::task::ready!(this.inner.poll(cx))?;
        let config = &**this.config;
        if res.extensions().get::<super::AeoDocument>().is_some() {
            res.headers_mut().remove(axum::http::header::SET_COOKIE);
        }
        if res.status() == StatusCode::UNAUTHORIZED
            && let Some(hint) = &config.resource_metadata
            && !res
                .headers()
                .contains_key(axum::http::header::WWW_AUTHENTICATE)
        {
            res.headers_mut()
                .insert(axum::http::header::WWW_AUTHENTICATE, hint.clone());
        }
        if this.flags.is_home
            && res.status().is_success()
            && let Some(link) = &config.home_link
        {
            res.headers_mut().append(LINK, link.clone());
        }
        if !(config.markdown && this.flags.readable && is_negotiable(&res)) {
            return Poll::Ready(Ok(res));
        }
        add_vary_accept(res.headers_mut());
        if !this.flags.wants_markdown {
            return Poll::Ready(Ok(res));
        }
        if this.flags.is_head {
            // HEAD carries the headers a GET would get; there is no body.
            if !too_large(&res, config.max_bytes) {
                markdown_headers(res.headers_mut(), config, None);
            }
            return Poll::Ready(Ok(res));
        }
        let config = Arc::clone(this.config);
        let mut convert: ConvertFuture = Box::pin(async move { to_markdown(res, &config).await });
        let poll = convert.as_mut().poll(cx);
        *this.convert = Some(convert);
        poll.map(Ok)
    }
}

/// `true` when `Accept` names `text/markdown` with a weight at least as
/// high as the HTML weight. The HTML weight comes from the most specific
/// range that covers HTML (`text/html`, then `text/*`, then `*/*`).
#[must_use]
pub fn prefers_markdown(headers: &HeaderMap) -> bool {
    let mut markdown = 0.0_f32;
    // (specificity, q) of the most specific range that covers HTML. RFC 9110
    // §12.5.1: `text/html` overrides `text/*`, which overrides `*/*`.
    let mut html = (0_u8, 0.0_f32);
    for value in headers.get_all(axum::http::header::ACCEPT) {
        let Ok(value) = value.to_str() else { continue };
        for range in value.split(',') {
            let mut parts = range.split(';');
            let media = parts.next().unwrap_or("").trim();
            let q = accept_weight(parts);
            if media.eq_ignore_ascii_case("text/markdown") {
                markdown = markdown.max(q);
                continue;
            }
            let specificity = if media.eq_ignore_ascii_case("text/html") {
                3
            } else if media.eq_ignore_ascii_case("text/*") {
                2
            } else if media == "*/*" {
                1
            } else {
                continue;
            };
            if specificity > html.0 || (specificity == html.0 && q > html.1) {
                html = (specificity, q);
            }
        }
    }
    // Markdown must be named: `*/*` alone still gets HTML.
    markdown > 0.0 && markdown >= html.1
}

/// The `q` weight of one media range (RFC 9110 §12.4.2). A missing `q` is
/// 1; a `q` that does not parse is 0.
fn accept_weight<'a>(params: impl Iterator<Item = &'a str>) -> f32 {
    for param in params {
        let Some((key, value)) = param.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("q") {
            return value
                .trim()
                .parse::<f32>()
                .map_or(0.0, |q| q.clamp(0.0, 1.0));
        }
    }
    1.0
}

/// A whole `200` HTML document: not encoded, not a range, not a download.
fn is_negotiable(res: &Response) -> bool {
    let headers = res.headers();
    res.status() == StatusCode::OK
        && headers
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| {
                ct.split(';')
                    .next()
                    .is_some_and(|m| m.trim().eq_ignore_ascii_case("text/html"))
            })
        && !headers.contains_key(CONTENT_ENCODING)
        && !headers.contains_key(CONTENT_RANGE)
        && !headers
            .get(CONTENT_DISPOSITION)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                v.trim_start()
                    .to_ascii_lowercase()
                    .starts_with("attachment")
            })
}

fn add_vary_accept(headers: &mut HeaderMap) {
    let present = headers.get_all(VARY).iter().any(|v| {
        v.to_str().is_ok_and(|v| {
            v.split(',')
                .any(|p| p.trim() == "*" || p.trim().eq_ignore_ascii_case("accept"))
        })
    });
    if !present {
        headers.append(VARY, HeaderValue::from_static("Accept"));
    }
}

/// `true` when the body is known to be larger than `max`: from
/// `Content-Length`, else from an exact body size. `GET` and `HEAD` use the
/// same test, so `HEAD` describes the representation `GET` sends.
fn too_large(res: &Response, max: usize) -> bool {
    use http_body::Body as _;

    res.headers()
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .or_else(|| res.body().size_hint().exact())
        .is_some_and(|len| len > max as u64)
}

async fn to_markdown(res: Response, config: &NegotiateConfig) -> Response {
    if too_large(&res, config.max_bytes) {
        return res;
    }
    let (mut parts, body) = res.into_parts();
    let bytes = match collect_limited(body, config.max_bytes).await {
        Ok(bytes) => bytes,
        Err(body) => return Response::from_parts(parts, body),
    };
    let markdown = if bytes.len() > BLOCKING_THRESHOLD {
        // A large page converts off the async worker threads.
        let html = bytes.clone();
        match crate::time::spawn_blocking(move || {
            super::markdown::html_to_markdown(&String::from_utf8_lossy(&html))
        })
        .await
        {
            Ok(markdown) => markdown,
            Err(_) => return Response::from_parts(parts, Body::from(bytes)),
        }
    } else {
        super::markdown::html_to_markdown(&String::from_utf8_lossy(&bytes))
    };
    markdown_headers(
        &mut parts.headers,
        config,
        Some(super::markdown::estimate_tokens(&markdown)),
    );
    Response::from_parts(parts, Body::from(markdown))
}

/// Pages larger than this convert on a blocking thread.
const BLOCKING_THRESHOLD: usize = 256 * 1024;

/// Turn HTML response headers into Markdown ones. `tokens` is `None` for
/// `HEAD`, which has no body to count.
fn markdown_headers(headers: &mut HeaderMap, config: &NegotiateConfig, tokens: Option<usize>) {
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/markdown; charset=utf-8"),
    );
    headers.remove(CONTENT_LENGTH);
    headers.remove(LAST_MODIFIED);
    // These describe the HTML bytes, not the Markdown.
    for name in ["content-digest", "repr-digest", "digest", "content-md5"] {
        headers.remove(name);
    }
    headers.remove(axum::http::header::ACCEPT_RANGES);
    if let Some(etag) = headers.get(ETAG).and_then(|v| v.to_str().ok()) {
        let opaque = etag.trim_start_matches("W/").trim_matches('"');
        match HeaderValue::from_str(&format!("W/\"{opaque}-md\"")) {
            Ok(v) => {
                headers.insert(ETAG, v);
            }
            Err(_) => {
                headers.remove(ETAG);
            }
        }
    }
    // Some CDNs ignore `Vary` on HTML. With no cache policy from the app,
    // keep the Markdown copy out of shared caches.
    if !headers.contains_key(axum::http::header::CACHE_CONTROL) {
        headers.insert(
            axum::http::header::CACHE_CONTROL,
            HeaderValue::from_static("private"),
        );
    }
    if let Some(tokens) = tokens {
        headers.insert("x-markdown-tokens", HeaderValue::from(tokens));
    }
    if let Some(signal) = &config.content_signal {
        headers.insert("content-signal", signal.clone());
    }
}

/// Read `body` up to `limit` bytes. Over the limit, return a body that
/// sends the bytes read, then the remaining bytes.
async fn collect_limited(mut body: Body, limit: usize) -> Result<Bytes, Body> {
    let mut chunks: Vec<Bytes> = Vec::new();
    let mut total = 0usize;
    loop {
        match body.frame().await {
            None => break,
            Some(Ok(frame)) => {
                let Ok(data) = frame.into_data() else {
                    continue;
                };
                total += data.len();
                chunks.push(data);
                if total > limit {
                    let head = futures::stream::iter(chunks.into_iter().map(Ok::<_, axum::Error>));
                    return Err(Body::from_stream(head.chain(body.into_data_stream())));
                }
            }
            Some(Err(err)) => {
                let head = futures::stream::iter(chunks.into_iter().map(Ok::<_, axum::Error>));
                return Err(Body::from_stream(
                    head.chain(futures::stream::once(async { Err(err) })),
                ));
            }
        }
    }
    let mut buf = Vec::with_capacity(total);
    for chunk in chunks {
        buf.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accept(v: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::ACCEPT,
            HeaderValue::from_str(v).unwrap(),
        );
        h
    }

    #[test]
    fn accept_ranking() {
        assert!(prefers_markdown(&accept("text/markdown")));
        assert!(prefers_markdown(&accept("text/markdown, text/html;q=0.9")));
        assert!(prefers_markdown(&accept("text/html;q=0.5, text/markdown")));
        assert!(!prefers_markdown(&accept("text/html, text/markdown;q=0.5")));
        assert!(!prefers_markdown(&accept("text/markdown;q=0")));
        assert!(!prefers_markdown(&accept("*/*")));
        assert!(!prefers_markdown(&accept("text/markdown;q=0.5, */*")));
        assert!(!prefers_markdown(&accept("text/markdown;q=0.5, text/*")));
        assert!(prefers_markdown(&accept("text/markdown, */*;q=0.8")));
        assert!(prefers_markdown(&accept(
            "text/markdown;Q=0.9, text/html;q=0.5"
        )));
        assert!(!prefers_markdown(&accept("text/markdown;q=abc")));
        assert!(!prefers_markdown(&HeaderMap::new()));
    }

    #[test]
    fn the_most_specific_range_sets_the_html_weight() {
        // RFC 9110 §12.5.1: `text/html;q=0` overrides `*/*`.
        assert!(prefers_markdown(&accept(
            "text/html;q=0, text/markdown;q=0.5, */*;q=1"
        )));
        assert!(prefers_markdown(&accept(
            "text/*;q=0.2, text/markdown;q=0.5, */*"
        )));
        assert!(!prefers_markdown(&accept(
            "text/html;q=0.9, text/markdown;q=0.5, */*;q=0.1"
        )));
    }

    #[test]
    fn conversion_drops_metadata_about_the_html_bytes() {
        let mut h = HeaderMap::new();
        for name in [
            "content-digest",
            "repr-digest",
            "digest",
            "content-md5",
            "accept-ranges",
        ] {
            h.insert(
                axum::http::HeaderName::from_static(name),
                HeaderValue::from_static("x"),
            );
        }
        let config = NegotiateConfig {
            markdown: true,
            max_bytes: 1024,
            home_link: None,
            content_signal: None,
            resource_metadata: None,
        };
        markdown_headers(&mut h, &config, Some(1));
        for name in [
            "content-digest",
            "repr-digest",
            "digest",
            "content-md5",
            "accept-ranges",
        ] {
            assert!(h.get(name).is_none(), "{name}");
        }
    }

    #[tokio::test]
    async fn collect_limited_replays_an_oversized_body() {
        let body = Body::from_stream(futures::stream::iter(
            ["aaaa", "bbbb", "cccc"].map(|s| Ok::<_, std::io::Error>(Bytes::from(s))),
        ));
        let Err(body) = collect_limited(body, 6).await else {
            panic!("over the limit")
        };
        let all = body.collect().await.unwrap().to_bytes();
        assert_eq!(&all[..], b"aaaabbbbcccc");
        let ok = collect_limited(Body::from("abc"), 6).await.unwrap();
        assert_eq!(&ok[..], b"abc");
    }
}
