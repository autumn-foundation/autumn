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
    IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED, LINK, VARY,
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
    /// The agent `Link` header of each home page: `/`, or `/{locale}` for
    /// each locale when the root is locale-prefixed.
    pub home_links: Vec<(String, HeaderValue)>,
    /// `Content-Signal` header for Markdown responses.
    pub content_signal: Option<HeaderValue>,
    /// `WWW-Authenticate` value for a `401` (RFC 9728 `resource_metadata`).
    pub resource_metadata: Option<HeaderValue>,
    /// Build the `401` hint from the request `Host` (OAuth is set, `[seo]
    /// base_url` is not), as the metadata document itself is.
    pub resource_metadata_from_host: bool,
}

impl NegotiateConfig {
    /// The index of `path` in [`Self::home_links`], when it is a home page.
    fn home(&self, path: &str) -> Option<usize> {
        self.home_links.iter().position(|(home, _)| home == path)
    }
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

    fn call(&mut self, mut req: Request<Body>) -> Self::Future {
        let is_get = req.method() == Method::GET;
        let readable = is_get || req.method() == Method::HEAD;
        let flags = Flags {
            home: if readable {
                self.config.home(req.uri().path())
            } else {
                None
            },
            readable,
            wants_markdown: self.config.markdown && readable && prefers_markdown(req.headers()),
            is_head: !is_get && readable,
        };
        // A Markdown request's validators are checked here, against the
        // representation sent (Markdown, or the response as it is), not by
        // the handler against its HTML.
        let validators = if flags.wants_markdown {
            Validators::take(req.headers_mut())
        } else {
            None
        };
        let host = if self.config.resource_metadata_from_host {
            req.headers().get(axum::http::header::HOST).cloned()
        } else {
            None
        };
        // The future holds only the inner future. It does not clone the
        // service. It boxes only a page that it converts.
        NegotiateFuture {
            inner: self.inner.call(req),
            config: Arc::clone(&self.config),
            flags,
            validators,
            host,
            convert: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
#[allow(clippy::struct_excessive_bools)] // request facts, read once
struct Flags {
    /// The request's entry in `home_links`.
    home: Option<usize>,
    readable: bool,
    wants_markdown: bool,
    is_head: bool,
}

type ConvertFuture = Pin<Box<dyn Future<Output = Response> + Send>>;

/// The conditional headers of a Markdown request.
#[derive(Debug)]
struct Validators {
    if_none_match: Vec<HeaderValue>,
    if_modified_since: Option<HeaderValue>,
}

impl Validators {
    /// Remove the request's validators, if it has any.
    fn take(headers: &mut HeaderMap) -> Option<Box<Self>> {
        let if_none_match: Vec<HeaderValue> = match headers.entry(IF_NONE_MATCH) {
            axum::http::header::Entry::Occupied(e) => e.remove_entry_mult().1.collect(),
            axum::http::header::Entry::Vacant(_) => Vec::new(),
        };
        let if_modified_since = headers.remove(IF_MODIFIED_SINCE);
        (!if_none_match.is_empty() || if_modified_since.is_some()).then(|| {
            Box::new(Self {
                if_none_match,
                if_modified_since,
            })
        })
    }

    /// `res` as a `304` when these validators match it.
    fn apply(&self, res: Response) -> Response {
        if res.status() == StatusCode::OK
            && crate::etag::validators_match(
                &self.if_none_match,
                self.if_modified_since.as_ref(),
                res.headers(),
            )
        {
            crate::etag::not_modified_from(res.headers())
        } else {
            res
        }
    }
}

pin_project_lite::pin_project! {
    /// Future of [`NegotiateService`].
    pub struct NegotiateFuture<F> {
        #[pin]
        inner: F,
        config: Arc<NegotiateConfig>,
        flags: Flags,
        validators: Option<Box<Validators>>,
        host: Option<HeaderValue>,
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
        if res.status() == StatusCode::UNAUTHORIZED {
            let hint = config.resource_metadata.clone().or_else(|| {
                config
                    .resource_metadata_from_host
                    .then(|| host_resource_metadata(this.host.as_ref()))
                    .flatten()
            });
            if let Some(hint) = hint {
                add_resource_metadata(res.headers_mut(), &hint);
            }
        }
        if res.status().is_success()
            && let Some((_, link)) = this.flags.home.and_then(|i| config.home_links.get(i))
        {
            res.headers_mut().append(LINK, link.clone());
        }
        let validators = this.validators.take();
        let checked = |res: Response| match &validators {
            Some(v) => v.apply(res),
            None => res,
        };
        if !(config.markdown && this.flags.readable && is_negotiable(&res)) {
            return Poll::Ready(Ok(checked(res)));
        }
        add_vary_accept(res.headers_mut());
        if !this.flags.wants_markdown {
            return Poll::Ready(Ok(res));
        }
        if this.flags.is_head {
            // HEAD carries the headers a GET would get; there is no body.
            // Only a known length shows that GET's body fits: a stream of
            // unknown length may outgrow the limit, and GET then sends HTML.
            if known_to_fit(&res, config.max_bytes) {
                markdown_headers(res.headers_mut(), config, None);
            }
            return Poll::Ready(Ok(checked(res)));
        }
        let config = Arc::clone(this.config);
        let mut convert: ConvertFuture = Box::pin(async move {
            let res = to_markdown(res, &config).await;
            match validators {
                Some(v) => v.apply(res),
                None => res,
            }
        });
        let poll = convert.as_mut().poll(cx);
        *this.convert = Some(convert);
        poll.map(Ok)
    }
}

/// The `401` hint for a site with no `[seo] base_url`: the metadata URL at
/// the request's own origin.
fn host_resource_metadata(host: Option<&HeaderValue>) -> Option<HeaderValue> {
    let origin = super::documents::Origin::resolve(None, host.and_then(|h| h.to_str().ok()));
    HeaderValue::from_str(&format!(
        "Bearer resource_metadata=\"{}\"",
        origin.url(super::documents::OAUTH_RESOURCE_PATH)
    ))
    .ok()
}

/// Point a `401` at the RFC 9728 metadata. With no challenge, `hint` (a
/// `Bearer resource_metadata=...` challenge) is the challenge. A `Bearer`
/// challenge of the handler gets the `resource_metadata` parameter, and
/// keeps its own. Any other scheme is left alone.
fn add_resource_metadata(headers: &mut HeaderMap, hint: &HeaderValue) {
    use axum::http::header::WWW_AUTHENTICATE;

    if !headers.contains_key(WWW_AUTHENTICATE) {
        headers.insert(WWW_AUTHENTICATE, hint.clone());
        return;
    }
    let Some(param) = hint.to_str().ok().and_then(|h| h.strip_prefix("Bearer ")) else {
        return;
    };
    let merged: Vec<HeaderValue> = headers
        .get_all(WWW_AUTHENTICATE)
        .iter()
        .map(|v| {
            v.to_str()
                .ok()
                .and_then(|s| with_bearer_param(s, param))
                .and_then(|s| HeaderValue::from_str(&s).ok())
                .unwrap_or_else(|| v.clone())
        })
        .collect();
    headers.remove(WWW_AUTHENTICATE);
    for v in merged {
        headers.append(WWW_AUTHENTICATE, v);
    }
}

/// `challenges` with `param` added to its `Bearer` challenge, or `None` when
/// it has no `Bearer` challenge or already names `resource_metadata`.
fn with_bearer_param(challenges: &str, param: &str) -> Option<String> {
    if challenges
        .to_ascii_lowercase()
        .contains("resource_metadata")
    {
        return None;
    }
    // Split on commas outside quoted strings.
    let mut items = Vec::new();
    let (mut start, mut quoted, mut escaped) = (0, false, false);
    for (i, c) in challenges.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ',' if !quoted => {
                items.push(challenges[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    items.push(challenges[start..].trim());
    // An item opens a challenge when its first token is not followed by `=`.
    let opens = |item: &str| {
        let token_end = item
            .find(|c: char| c == '=' || c.is_whitespace())
            .unwrap_or(item.len());
        !item[token_end..].trim_start().starts_with('=')
    };
    let first = items.iter().position(|item| {
        opens(item)
            && item
                .split_whitespace()
                .next()
                .is_some_and(|s| s.eq_ignore_ascii_case("Bearer"))
    })?;
    let end = items[first + 1..]
        .iter()
        .position(|item| opens(item))
        .map_or(items.len(), |n| first + 1 + n);
    let mut out: Vec<String> = items.iter().map(|s| (*s).to_owned()).collect();
    if out[first].trim().eq_ignore_ascii_case("Bearer") {
        out[first] = format!("Bearer {param}");
    } else {
        out.insert(end, param.to_owned());
    }
    Some(out.join(", "))
}

/// Rewrite the HTML `ETag` to its Markdown form, `W/"<tag>-md"`.
fn markdown_etag(headers: &mut HeaderMap) {
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

/// The body length: `Content-Length`, else an exact body size.
fn known_len(res: &Response) -> Option<u64> {
    use http_body::Body as _;

    res.headers()
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .or_else(|| res.body().size_hint().exact())
}

/// `true` when the body is known to be larger than `max`.
fn too_large(res: &Response, max: usize) -> bool {
    known_len(res).is_some_and(|len| len > max as u64)
}

/// `true` when a `HEAD` response's `GET` body is known to fit in `max`.
/// The `HEAD` body is already empty, so only `Content-Length`, or a
/// non-empty exact size, says how long the `GET` body is.
fn known_to_fit(res: &Response, max: usize) -> bool {
    known_len(res).is_some_and(|len| len > 0 && len <= max as u64)
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
    markdown_etag(headers);
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
                    // Replay every frame, trailers too: the response stays HTML.
                    let head = futures::stream::iter(
                        chunks
                            .into_iter()
                            .map(|b| Ok::<_, axum::Error>(http_body::Frame::data(b))),
                    );
                    let rest = http_body_util::BodyStream::new(body);
                    return Err(Body::new(http_body_util::StreamBody::new(head.chain(rest))));
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
    fn localized_home_pages_are_home() {
        let link = |s: &'static str| HeaderValue::from_static(s);
        let config = NegotiateConfig {
            home_links: vec![
                ("/en".to_owned(), link("<x>; rel=\"alternate\"")),
                ("/fr".to_owned(), link("<y>; rel=\"alternate\"")),
            ],
            ..NegotiateConfig::default()
        };
        assert_eq!(config.home("/fr"), Some(1));
        assert_eq!(config.home("/"), None);
        assert_eq!(config.home("/en/x"), None);
    }

    #[test]
    fn validators_are_checked_against_the_representation_sent() {
        let mut req = HeaderMap::new();
        req.insert(IF_NONE_MATCH, HeaderValue::from_static("W/\"v1-md\""));
        req.insert(IF_MODIFIED_SINCE, HeaderValue::from_static("x"));
        let v = Validators::take(&mut req).expect("validators");
        assert!(req.get(IF_NONE_MATCH).is_none() && req.get(IF_MODIFIED_SINCE).is_none());

        let md = |etag: &'static str| {
            let mut res = Response::new(Body::from("body"));
            res.headers_mut()
                .insert(ETAG, HeaderValue::from_static(etag));
            res
        };
        assert_eq!(
            v.apply(md("W/\"v1-md\"")).status(),
            StatusCode::NOT_MODIFIED
        );
        // The HTML tag is another representation: no 304.
        assert_eq!(v.apply(md("\"v1\"")).status(), StatusCode::OK);
        assert!(Validators::take(&mut HeaderMap::new()).is_none());
    }

    #[test]
    fn resource_metadata_joins_the_handlers_bearer_challenge() {
        let p = "resource_metadata=\"https://s/m\"";
        assert_eq!(
            with_bearer_param("Bearer realm=\"api\", error=\"invalid_token\"", p).as_deref(),
            Some(
                "Bearer realm=\"api\", error=\"invalid_token\", resource_metadata=\"https://s/m\""
            )
        );
        assert_eq!(
            with_bearer_param("Bearer", p).as_deref(),
            Some("Bearer resource_metadata=\"https://s/m\"")
        );
        // The parameter goes on the Bearer challenge, not the one after it.
        assert_eq!(
            with_bearer_param("Bearer realm=\"a, b\", Basic realm=\"x\"", p).as_deref(),
            Some("Bearer realm=\"a, b\", resource_metadata=\"https://s/m\", Basic realm=\"x\"")
        );
        assert_eq!(with_bearer_param("Basic realm=\"x\"", p), None);
        assert_eq!(
            with_bearer_param("Bearer resource_metadata=\"https://o\"", p),
            None
        );
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
            home_links: Vec::new(),
            content_signal: None,
            resource_metadata: None,
            resource_metadata_from_host: false,
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
    async fn collect_limited_keeps_trailers_of_an_oversized_body() {
        let mut trailers = HeaderMap::new();
        trailers.insert("x-digest", HeaderValue::from_static("abc"));
        let frames = futures::stream::iter([
            Ok::<_, std::io::Error>(http_body::Frame::data(Bytes::from("aaaa"))),
            Ok(http_body::Frame::data(Bytes::from("bbbb"))),
            Ok(http_body::Frame::trailers(trailers)),
        ]);
        let body = Body::new(http_body_util::StreamBody::new(frames));
        let Err(body) = collect_limited(body, 6).await else {
            panic!("over the limit")
        };
        let collected = body.collect().await.unwrap();
        assert_eq!(
            collected.trailers().and_then(|t| t.get("x-digest")),
            Some(&HeaderValue::from_static("abc"))
        );
        assert_eq!(&collected.to_bytes()[..], b"aaaabbbb");
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
