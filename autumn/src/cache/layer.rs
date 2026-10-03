//! Tower middleware that caches HTTP GET responses.
//!
//! Only caches `GET` requests that produce `200 OK` responses.
//! Non-GET methods and non-200 responses pass through untouched.
//!
//! # Usage
//!
//! ```rust,ignore
//! use autumn_web::prelude::*;
//! use autumn_web::cache::{CacheResponseLayer, MokaCache};
//!
//! let store = MokaCache::builder()
//!     .max_capacity(1000)
//!     .ttl(std::time::Duration::from_secs(300))
//!     .build();
//!
//! #[get("/users/{id}")]
//! #[intercept(CacheResponseLayer::from_cache(store))]
//! async fn get_user(Path(id): Path<i32>) -> Json<User> { ... }
//! ```

use std::convert::Infallible;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{Method, StatusCode};
use http::Request;
use http_body_util::BodyExt;
use tower::{Layer, Service};

use super::Cache;

/// A cached HTTP response: status, headers, and body bytes.
#[derive(Clone, serde::Deserialize, serde::Serialize)]
struct CachedResponse {
    status: u16,
    headers: Vec<CachedHeader>,
    body: Vec<u8>,
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
struct CachedHeader {
    name: String,
    value: Vec<u8>,
}

fn cached_response_from_parts(
    parts: &http::response::Parts,
    body: &bytes::Bytes,
) -> CachedResponse {
    let headers = parts
        .headers
        .iter()
        .map(|(name, value)| CachedHeader {
            name: name.as_str().to_owned(),
            value: value.as_bytes().to_vec(),
        })
        .collect();

    CachedResponse {
        status: parts.status.as_u16(),
        headers,
        body: body.to_vec(),
    }
}

fn cached_response_into_response(cached: CachedResponse) -> Option<axum::response::Response> {
    let status = StatusCode::from_u16(cached.status).ok()?;
    let mut builder = axum::response::Response::builder().status(status);
    let headers = builder.headers_mut()?;

    for cached_header in cached.headers {
        let name = http::HeaderName::from_bytes(cached_header.name.as_bytes()).ok()?;
        let value = http::HeaderValue::from_bytes(&cached_header.value).ok()?;
        headers.append(name, value);
    }

    builder.body(Body::from(cached.body)).ok()
}

/// Tower layer that caches HTTP GET responses.
///
/// Wrap around a handler via `#[intercept(CacheResponseLayer::from_cache(store))]`
/// or construct manually and apply with `.layer()`.
///
/// Caching rules:
/// - Only `GET` requests are cached.
/// - Only `200 OK` responses are cached.
/// - The cache key is the request URI path + query string.
///
/// # Only wrap responses that are the same for every visitor
///
/// That last rule is the whole contract: the key is the URI and *nothing
/// else*. This layer does not read `Vary`, does not read `Cache-Control`,
/// and does not look at cookies — so whatever the first visitor's request
/// produced for a URI is replayed verbatim to everyone else who asks for it
/// until the entry expires. Wrapping a handler whose body depends on who is
/// asking — a session, a signed-in user, a
/// [`Consent`](crate::consent::Consent) decision, a per-visitor CSRF token —
/// leaks one visitor's page to the next.
///
/// Stamping `Vary: Cookie` or `Cache-Control: private, no-store` on such a
/// response does not make it safe to wrap. Those headers instruct *HTTP*
/// caches — the browser, a CDN, a reverse proxy — and this layer is not one
/// of those; it sits inside the application, above the handler, and hits
/// before any of them are consulted.
///
/// So either keep per-visitor routes out of this layer entirely, or give the
/// cache a key that includes what the response varies on.
#[derive(Clone)]
pub struct CacheResponseLayer {
    store: Arc<dyn Cache>,
}

impl CacheResponseLayer {
    /// Create a layer backed by the given cache store.
    pub fn from_cache(store: impl Cache + 'static) -> Self {
        Self {
            store: Arc::new(store),
        }
    }

    /// Create from an existing `Arc<dyn Cache>`.
    pub fn from_shared(store: Arc<dyn Cache>) -> Self {
        Self { store }
    }

    /// Create from the global cache registered in `AppState`.
    ///
    /// Returns `None` when no global cache has been registered (i.e. the app
    /// is running with the default per-function Moka caches only).
    #[must_use]
    pub fn from_app(state: &crate::state::AppState) -> Option<Self> {
        state.cache().map(Self::from_shared)
    }
}

impl<S> Layer<S> for CacheResponseLayer {
    type Service = CacheResponseService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        CacheResponseService {
            inner,
            store: self.store.clone(),
        }
    }
}

/// The [`Service`] produced by [`CacheResponseLayer`].
#[derive(Clone)]
pub struct CacheResponseService<S> {
    inner: S,
    store: Arc<dyn Cache>,
}

impl<S> Service<Request<Body>> for CacheResponseService<S>
where
    S: Service<Request<Body>, Response = axum::response::Response, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send,
{
    type Response = axum::response::Response;
    type Error = Infallible;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        // Only cache GET requests
        if req.method() != Method::GET {
            return Box::pin(self.inner.call(req));
        }

        // ⚡ Bolt Optimization:
        // Format the key into a stack-allocated buffer to avoid a heap allocation
        // on every cache check. Fall back to allocating a String only if the URI
        // is exceptionally long.
        let mut buf = [0u8; 512];
        let cache_key_str = {
            let mut cursor = &mut buf[..];
            if std::io::Write::write_fmt(&mut cursor, format_args!("http:{}", req.uri())).is_ok() {
                let len = 512 - cursor.len();
                std::str::from_utf8(&buf[..len]).unwrap_or_default()
            } else {
                ""
            }
        };

        let store = self.store.clone();

        let cache_hit = if cache_key_str.is_empty() {
            // Fallback for very long URIs
            super::get_cached::<CachedResponse>(store.as_ref(), &format!("http:{}", req.uri()))
        } else {
            super::get_cached::<CachedResponse>(store.as_ref(), cache_key_str)
        };

        // Check for a cache hit
        if let Some(cached) = cache_hit
            && let Some(resp) = cached_response_into_response(cached)
        {
            return Box::pin(async move { Ok(resp) });
        }

        // Cache miss — call the inner service
        let mut inner = self.inner.clone();
        let cache_key = if cache_key_str.is_empty() {
            format!("http:{}", req.uri())
        } else {
            cache_key_str.to_owned()
        };

        Box::pin(async move {
            let response = inner.call(req).await?;

            // Only cache 200 OK responses
            if response.status() != StatusCode::OK {
                return Ok(response);
            }

            let (parts, body) = response.into_parts();

            // Buffer the body
            let Ok(collected) = body.collect().await else {
                let resp = axum::response::Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Body::empty())
                    .expect("infallible response builder");
                return Ok(resp);
            };
            let body_bytes = collected.to_bytes();

            // Store in cache
            let cached = cached_response_from_parts(&parts, &body_bytes);
            super::insert_cached(store.as_ref(), &cache_key, cached, None);

            // Reconstruct the response
            let response = axum::response::Response::from_parts(parts, Body::from(body_bytes));
            Ok(response)
        })
    }
}

#[cfg(all(test, feature = "cache-moka"))]
mod tests {
    use super::*;
    use crate::cache::RawCacheBytes;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::{ServiceBuilder, ServiceExt};

    #[derive(Default)]
    struct RawOnlyCache {
        entries: Mutex<HashMap<String, Vec<u8>>>,
    }

    impl Cache for RawOnlyCache {
        fn get_value(&self, key: &str) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
            self.entries
                .lock()
                .expect("raw cache lock poisoned")
                .get(key)
                .cloned()
                .map(|bytes| Arc::new(RawCacheBytes(bytes)) as Arc<dyn std::any::Any + Send + Sync>)
        }

        fn insert_value(&self, _key: &str, _value: Arc<dyn std::any::Any + Send + Sync>) {}

        fn insert_raw_bytes(&self, key: &str, bytes: Vec<u8>, _ttl: Option<std::time::Duration>) {
            self.entries
                .lock()
                .expect("raw cache lock poisoned")
                .insert(key.to_owned(), bytes);
        }

        fn invalidate(&self, key: &str) {
            self.entries
                .lock()
                .expect("raw cache lock poisoned")
                .remove(key);
        }

        fn clear(&self) {
            self.entries
                .lock()
                .expect("raw cache lock poisoned")
                .clear();
        }
    }

    /// Build a test service that returns a fixed body and counts calls.
    fn counting_service(
        counter: Arc<AtomicUsize>,
        body: &'static str,
    ) -> impl Service<
        Request<Body>,
        Response = axum::response::Response,
        Error = Infallible,
        Future = impl std::future::Future<Output = Result<axum::response::Response, Infallible>> + Send,
    > + Clone
    + Send
    + 'static {
        let body = body.to_owned();
        tower::service_fn(move |_req: Request<Body>| {
            let counter = counter.clone();
            let body = body.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(axum::response::Response::builder()
                    .status(StatusCode::OK)
                    .body(Body::from(body))
                    .expect("infallible response builder"))
            }
        })
    }

    #[tokio::test]
    async fn caches_get_responses() {
        let store = super::super::MokaCache::new(100, None);
        let counter = Arc::new(AtomicUsize::new(0));

        let mut svc = ServiceBuilder::new()
            .layer(CacheResponseLayer::from_cache(store))
            .service(counting_service(counter.clone(), "hello"));

        // First request — cache miss
        let req = Request::get("/test")
            .body(Body::empty())
            .expect("infallible response builder");
        let resp = svc
            .ready()
            .await
            .expect("infallible response builder")
            .call(req)
            .await
            .expect("infallible response builder");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .expect("infallible response builder")
            .to_bytes();
        assert_eq!(body.as_ref(), b"hello");
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        // Second request — cache hit, inner service NOT called
        let req = Request::get("/test")
            .body(Body::empty())
            .expect("infallible response builder");
        let resp = svc
            .ready()
            .await
            .expect("infallible response builder")
            .call(req)
            .await
            .expect("infallible response builder");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .expect("infallible response builder")
            .to_bytes();
        assert_eq!(body.as_ref(), b"hello");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "inner should not be called again"
        );
    }

    #[tokio::test]
    async fn caches_get_responses_with_raw_byte_backends() {
        let store = Arc::new(RawOnlyCache::default());
        let counter = Arc::new(AtomicUsize::new(0));

        let inner = {
            let counter = counter.clone();
            tower::service_fn(move |_req: Request<Body>| {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, Infallible>(
                        axum::response::Response::builder()
                            .status(StatusCode::OK)
                            .header("x-cache-test", "persisted")
                            .body(Body::from("redis-like"))
                            .expect("infallible response builder"),
                    )
                }
            })
        };

        let mut svc = ServiceBuilder::new()
            .layer(CacheResponseLayer::from_shared(store))
            .service(inner);

        let req = Request::get("/redis-backed")
            .body(Body::empty())
            .expect("infallible response builder");
        let resp = svc
            .ready()
            .await
            .expect("infallible response builder")
            .call(req)
            .await
            .expect("infallible response builder");
        assert_eq!(resp.status(), StatusCode::OK);

        let req = Request::get("/redis-backed")
            .body(Body::empty())
            .expect("infallible response builder");
        let resp = svc
            .ready()
            .await
            .expect("infallible response builder")
            .call(req)
            .await
            .expect("infallible response builder");

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("x-cache-test")
                .and_then(|v| v.to_str().ok()),
            Some("persisted")
        );
        let body = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .expect("infallible response builder")
            .to_bytes();
        assert_eq!(body.as_ref(), b"redis-like");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "raw-byte backends should cache HTTP responses"
        );
    }

    #[tokio::test]
    async fn does_not_cache_post_requests() {
        let store = super::super::MokaCache::new(100, None);
        let counter = Arc::new(AtomicUsize::new(0));

        let mut svc = ServiceBuilder::new()
            .layer(CacheResponseLayer::from_cache(store))
            .service(counting_service(counter.clone(), "created"));

        let req = Request::post("/items")
            .body(Body::empty())
            .expect("infallible response builder");
        let _resp = svc
            .ready()
            .await
            .expect("infallible response builder")
            .call(req)
            .await
            .expect("infallible response builder");
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        let req = Request::post("/items")
            .body(Body::empty())
            .expect("infallible response builder");
        let _resp = svc
            .ready()
            .await
            .expect("infallible response builder")
            .call(req)
            .await
            .expect("infallible response builder");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            2,
            "POST should not be cached"
        );
    }

    #[tokio::test]
    async fn does_not_cache_non_200_responses() {
        let store = super::super::MokaCache::new(100, None);
        let counter = Arc::new(AtomicUsize::new(0));

        let svc_inner = {
            let counter = counter.clone();
            tower::service_fn(move |_req: Request<Body>| {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, Infallible>(
                        axum::response::Response::builder()
                            .status(StatusCode::NOT_FOUND)
                            .body(Body::from("not found"))
                            .expect("infallible response builder"),
                    )
                }
            })
        };

        let mut svc = ServiceBuilder::new()
            .layer(CacheResponseLayer::from_cache(store))
            .service(svc_inner);

        let req = Request::get("/missing")
            .body(Body::empty())
            .expect("infallible response builder");
        let resp = svc
            .ready()
            .await
            .expect("infallible response builder")
            .call(req)
            .await
            .expect("infallible response builder");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let req = Request::get("/missing")
            .body(Body::empty())
            .expect("infallible response builder");
        let resp = svc
            .ready()
            .await
            .expect("infallible response builder")
            .call(req)
            .await
            .expect("infallible response builder");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            counter.load(Ordering::SeqCst),
            2,
            "404 should not be cached"
        );
    }

    #[tokio::test]
    async fn different_uris_cached_separately() {
        let store = super::super::MokaCache::new(100, None);
        let counter = Arc::new(AtomicUsize::new(0));

        let mut svc = ServiceBuilder::new()
            .layer(CacheResponseLayer::from_cache(store))
            .service(counting_service(counter.clone(), "ok"));

        let req = Request::get("/a")
            .body(Body::empty())
            .expect("infallible response builder");
        let _resp = svc
            .ready()
            .await
            .expect("infallible response builder")
            .call(req)
            .await
            .expect("infallible response builder");
        let req = Request::get("/b")
            .body(Body::empty())
            .expect("infallible response builder");
        let _resp = svc
            .ready()
            .await
            .expect("infallible response builder")
            .call(req)
            .await
            .expect("infallible response builder");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            2,
            "different URIs should miss"
        );

        // But repeating /a should hit
        let req = Request::get("/a")
            .body(Body::empty())
            .expect("infallible response builder");
        let _resp = svc
            .ready()
            .await
            .expect("infallible response builder")
            .call(req)
            .await
            .expect("infallible response builder");
        assert_eq!(counter.load(Ordering::SeqCst), 2, "/a should be cached");
    }

    #[test]
    fn from_shared_accepts_arc() {
        let store = Arc::new(super::super::MokaCache::new(100, None));
        // Just verify from_shared compiles and the layer can be used
        let _layer = CacheResponseLayer::from_shared(store);
    }

    #[tokio::test]
    async fn caches_get_responses_very_long_uri() {
        let store = super::super::MokaCache::new(100, None);
        let counter = Arc::new(AtomicUsize::new(0));

        let mut svc = ServiceBuilder::new()
            .layer(CacheResponseLayer::from_cache(store))
            .service(counting_service(counter.clone(), "hello"));

        let long_uri = format!("/test/{}", "a".repeat(1000));

        let req1 = Request::get(&long_uri)
            .body(Body::empty())
            .expect("infallible response builder");

        let resp1 = svc
            .ready()
            .await
            .expect("infallible response builder")
            .call(req1)
            .await
            .expect("infallible response builder");

        assert_eq!(resp1.status(), StatusCode::OK);
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        let req2 = Request::get(&long_uri)
            .body(Body::empty())
            .expect("infallible response builder");

        let resp2 = svc
            .ready()
            .await
            .expect("infallible response builder")
            .call(req2)
            .await
            .expect("infallible response builder");

        assert_eq!(resp2.status(), StatusCode::OK);
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "Should be cached despite long URI"
        );
    }
}
