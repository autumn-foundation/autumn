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
            wants_markdown: self.config.markdown && is_get && prefers_markdown(req.headers()),
        };
        // The future owns only the inner future: no clone of the inner
        // service, and no box unless the page is converted.
        NegotiateFuture {
            inner: self.inner.call(req),
            config: Arc::clone(&self.config),
            flags,
            convert: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Flags {
    is_home: bool,
    readable: bool,
    wants_markdown: bool,
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
        let config = Arc::clone(this.config);
        let mut convert: ConvertFuture = Box::pin(async move { to_markdown(res, &config).await });
        let poll = convert.as_mut().poll(cx);
        *this.convert = Some(convert);
        poll.map(Ok)
    }
}

/// `true` when `Accept` ranks `text/markdown` at least as high as
/// `text/html`, with a non-zero weight.
#[must_use]
pub fn prefers_markdown(headers: &HeaderMap) -> bool {
    let mut markdown = 0.0_f32;
    let mut html = 0.0_f32;
    for value in headers.get_all(axum::http::header::ACCEPT) {
        let Ok(value) = value.to_str() else { continue };
        for range in value.split(',') {
            let mut parts = range.split(';');
            let media = parts.next().unwrap_or("").trim();
            let q = parts
                .filter_map(|p| p.trim().strip_prefix("q="))
                .find_map(|q| q.trim().parse::<f32>().ok())
                .unwrap_or(1.0);
            if media.eq_ignore_ascii_case("text/markdown") {
                markdown = markdown.max(q);
            } else if media.eq_ignore_ascii_case("text/html") {
                html = html.max(q);
            }
        }
    }
    markdown > 0.0 && markdown >= html
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

async fn to_markdown(res: Response, config: &NegotiateConfig) -> Response {
    let declared_len = res
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok());
    if declared_len.is_some_and(|len| len > config.max_bytes) {
        return res;
    }
    let (mut parts, body) = res.into_parts();
    let bytes = match collect_limited(body, config.max_bytes).await {
        Ok(bytes) => bytes,
        Err(body) => return Response::from_parts(parts, body),
    };
    let markdown = super::markdown::html_to_markdown(&String::from_utf8_lossy(&bytes));
    let headers = &mut parts.headers;
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/markdown; charset=utf-8"),
    );
    headers.remove(CONTENT_LENGTH);
    headers.remove(LAST_MODIFIED);
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
    headers.insert(
        "x-markdown-tokens",
        HeaderValue::from(super::markdown::estimate_tokens(&markdown)),
    );
    if let Some(signal) = &config.content_signal {
        headers.insert("content-signal", signal.clone());
    }
    Response::from_parts(parts, Body::from(markdown))
}

/// Read `body` up to `limit` bytes. Past the limit, give back a body that
/// replays the read bytes and streams the rest.
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
        assert!(!prefers_markdown(&HeaderMap::new()));
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
