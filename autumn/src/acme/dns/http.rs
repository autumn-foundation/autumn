//! The minimal HTTP seam the API-backed DNS providers speak through
//! (issue #1620).
//!
//! Cloudflare and Route 53 are ordinary HTTPS APIs. Rather than let each
//! provider reach for a client directly, both go through [`HttpTransport`], so
//! the request each one *builds* — headers, path, body, and the signature over
//! them — is unit-testable without a network, and so a failing response can be
//! turned into an operator-facing message in exactly one place.
//!
//! The production implementation is [`ReqwestTransport`], over the `reqwest`
//! client already vendored in the workspace (rustls, no second TLS stack).

use std::collections::BTreeMap;

use futures::future::BoxFuture;

/// An outbound HTTP request built by a DNS provider.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HttpRequest {
    /// HTTP method (`GET`, `POST`, `DELETE`).
    pub method: &'static str,
    /// Absolute URL.
    pub url: String,
    /// Request headers. A `BTreeMap` so the order is deterministic — Route 53's
    /// `SigV4` canonical request depends on sorted header names.
    pub headers: BTreeMap<String, String>,
    /// Request body (empty for `GET`/`DELETE`).
    pub body: String,
}

/// Header names whose values are credentials and must never be rendered.
const SECRET_HEADERS: [&str; 2] = ["authorization", "x-amz-security-token"];

impl std::fmt::Debug for HttpRequest {
    /// Renders every header EXCEPT the credential-bearing ones, whose values
    /// show as `<redacted>`.
    ///
    /// Nothing in the provider paths debug-prints a request today, but a request
    /// carrying `Authorization: Bearer <token>` is one stray `{:?}` away from a
    /// token in the logs — and the derived `Debug` would print it. Redacting
    /// here makes that impossible rather than merely unlikely (#1620).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let headers: BTreeMap<&str, &str> = self
            .headers
            .iter()
            .map(|(name, value)| {
                let redact = SECRET_HEADERS.contains(&name.to_ascii_lowercase().as_str());
                (
                    name.as_str(),
                    if redact { "<redacted>" } else { value.as_str() },
                )
            })
            .collect();
        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("headers", &headers)
            .field("body", &self.body)
            .finish()
    }
}

impl HttpRequest {
    /// A request with no headers and an empty body.
    #[must_use]
    pub fn new(method: &'static str, url: impl Into<String>) -> Self {
        Self {
            method,
            url: url.into(),
            headers: BTreeMap::new(),
            body: String::new(),
        }
    }

    /// Set a header (builder style).
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }

    /// Set the body (builder style).
    #[must_use]
    pub fn body(mut self, body: impl Into<String>) -> Self {
        self.body = body.into();
        self
    }
}

/// An HTTP response as the providers need it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HttpResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response body.
    pub body: String,
}

impl HttpResponse {
    /// Whether the status is 2xx.
    #[must_use]
    pub const fn is_success(&self) -> bool {
        self.status >= 200 && self.status < 300
    }
}

/// Sends [`HttpRequest`]s. Implemented by [`ReqwestTransport`] in production and
/// by a recording fake in tests.
pub trait HttpTransport: Send + Sync {
    /// Send `request` and return the response.
    ///
    /// # Errors
    ///
    /// Returns a transport-level message (DNS failure, TLS failure, timeout).
    /// A non-2xx status is a successful send, not an error.
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>>;
}

/// The production [`HttpTransport`], over `reqwest`.
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    /// Build a transport with a bounded per-request timeout.
    ///
    /// # Errors
    ///
    /// Returns a message if the HTTP client cannot be constructed.
    pub fn new(timeout: std::time::Duration) -> Result<Self, String> {
        // A DNS provider API call must never park the renewal loop: without a
        // timeout a black-holed connection would hold the ACME order open until
        // the CA expired it, with nothing in the logs to say why.
        let client = reqwest::Client::builder()
            .timeout(timeout)
            // Never follow a redirect. `reqwest` strips `Authorization` on a
            // cross-origin hop, but it does not know `x-amz-security-token` is a
            // credential — so a `3xx` from the Route 53 endpoint would forward
            // the STS session token to whatever host it named. Neither the
            // Cloudflare v4 API nor Route 53 legitimately redirects these calls,
            // and this matches the workspace's other outbound clients.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| format!("failed to build the DNS provider HTTP client: {e}"))?;
        Ok(Self { client })
    }
}

impl HttpTransport for ReqwestTransport {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        Box::pin(async move {
            let method = reqwest::Method::from_bytes(request.method.as_bytes())
                .map_err(|e| format!("invalid HTTP method {}: {e}", request.method))?;
            let mut builder = self.client.request(method, &request.url);
            for (name, value) in &request.headers {
                builder = builder.header(name, value);
            }
            if !request.body.is_empty() {
                builder = builder.body(request.body.clone());
            }
            let response = builder.send().await.map_err(|e| {
                // `reqwest`'s Display walks the source chain but never renders
                // request headers, so an Authorization header cannot leak here.
                format!("DNS provider request to {} failed: {e}", request.url)
            })?;
            let status = response.status().as_u16();
            // Bounded: a misbehaving or compromised endpoint answering with a
            // multi-gigabyte body would otherwise OOM the renewal task, and the
            // body also feeds the operator-facing error message.
            let body = read_bounded(response).await?;
            Ok(HttpResponse { status, body })
        })
    }
}

/// The largest response body the providers will read.
///
/// Both APIs answer with small JSON/XML documents; 1 MiB is far past anything
/// legitimate and far short of anything that could exhaust memory.
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// Read a response body, refusing anything past [`MAX_RESPONSE_BYTES`].
async fn read_bounded(mut response: reqwest::Response) -> Result<String, String> {
    let too_large =
        || format!("the DNS provider answered with a body larger than {MAX_RESPONSE_BYTES} bytes");
    // Refuse an oversized body on its advertised length before buffering any of
    // it; the running total below is what catches a length that lied.
    if response
        .content_length()
        .is_some_and(|len| len > MAX_RESPONSE_BYTES as u64)
    {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("could not read the DNS provider response body: {e}"))?
    {
        if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(too_large());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// A recording [`HttpTransport`] for tests: every request is captured, and each
/// is answered from a script keyed by `"METHOD path"`.
///
/// Lives outside `#[cfg(test)]` so the provider modules' own tests can share it.
#[cfg(test)]
pub(crate) struct RecordingTransport {
    /// `"METHOD /path"` (query string excluded) → the responses to give, in
    /// order. The last response for a key is reused once the list is exhausted.
    script: std::sync::Mutex<std::collections::HashMap<String, Vec<HttpResponse>>>,
    sent: std::sync::Mutex<Vec<HttpRequest>>,
}

#[cfg(test)]
impl RecordingTransport {
    /// Build a transport answering `script`; an unscripted request answers 404.
    pub(crate) fn new(script: &[(&str, &str)]) -> std::sync::Arc<Self> {
        let mut map: std::collections::HashMap<String, Vec<HttpResponse>> =
            std::collections::HashMap::new();
        for (key, body) in script {
            map.entry((*key).to_owned())
                .or_default()
                .push(HttpResponse {
                    status: 200,
                    body: (*body).to_owned(),
                });
        }
        std::sync::Arc::new(Self {
            script: std::sync::Mutex::new(map),
            sent: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// Answer `key` with `status` and `body`, appended after any existing
    /// scripted responses for it.
    pub(crate) fn then(
        self: &std::sync::Arc<Self>,
        key: &str,
        status: u16,
        body: &str,
    ) -> std::sync::Arc<Self> {
        self.script
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(key.to_owned())
            .or_default()
            .push(HttpResponse {
                status,
                body: body.to_owned(),
            });
        std::sync::Arc::clone(self)
    }

    /// Every request sent, in order.
    pub(crate) fn sent(&self) -> Vec<HttpRequest> {
        self.sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// `"METHOD /path"` for each request sent, in order.
    pub(crate) fn calls(&self) -> Vec<String> {
        self.sent().iter().map(request_key).collect()
    }
}

/// The script key for a request: method plus path, query string excluded.
#[cfg(test)]
pub(crate) fn request_key(request: &HttpRequest) -> String {
    let path = request
        .url
        .split_once("://")
        .map_or(request.url.as_str(), |(_, rest)| rest);
    let path = path.find('/').map_or("/", |i| &path[i..]);
    let path = path.split('?').next().unwrap_or(path);
    format!("{} {path}", request.method)
}

#[cfg(test)]
impl HttpTransport for RecordingTransport {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        let key = request_key(&request);
        self.sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request);
        let mut script = self
            .script
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let response = match script.get_mut(&key) {
            // Keep the last scripted response once the list is down to one, so a
            // repeated poll does not have to be scripted N times.
            Some(responses) if responses.len() > 1 => responses.remove(0),
            Some(responses) => responses[0].clone(),
            None => HttpResponse {
                status: 404,
                body: format!(
                    "{{\"success\":false,\"errors\":[{{\"message\":\"unscripted {key}\"}}]}}"
                ),
            },
        };
        drop(script);
        Box::pin(async move { Ok(response) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_sets_method_headers_and_body() {
        let request = HttpRequest::new("POST", "https://api.example/x")
            .header("content-type", "application/json")
            .body("{}");
        assert_eq!(request.method, "POST");
        assert_eq!(request.headers["content-type"], "application/json");
        assert_eq!(request.body, "{}");
    }

    // A request carrying `Authorization: Bearer <token>` is one stray `{:?}`
    // away from a credential in the logs; the derived Debug would print it.
    #[test]
    fn debug_redacts_credential_headers_but_keeps_the_rest() {
        let request = HttpRequest::new("POST", "https://api.example/x")
            .header("authorization", "Bearer cf-live-token-DO-NOT-LEAK")
            .header("x-amz-security-token", "sts-session-secret")
            .header("content-type", "application/json")
            .body("{\"type\":\"TXT\"}");
        let rendered = format!("{request:?}");
        assert!(!rendered.contains("DO-NOT-LEAK"), "leaked: {rendered}");
        assert!(
            !rendered.contains("sts-session-secret"),
            "leaked: {rendered}"
        );
        assert_eq!(rendered.matches("<redacted>").count(), 2, "{rendered}");
        // …while everything an operator actually needs is still there.
        assert!(rendered.contains("application/json"), "{rendered}");
        assert!(rendered.contains("https://api.example/x"), "{rendered}");
        assert!(rendered.contains("POST"), "{rendered}");
    }

    #[test]
    fn success_is_2xx_only() {
        for (status, expected) in [
            (200, true),
            (201, true),
            (299, true),
            (300, false),
            (403, false),
            (500, false),
        ] {
            let response = HttpResponse {
                status,
                body: String::new(),
            };
            assert_eq!(response.is_success(), expected, "status {status}");
        }
    }
}
