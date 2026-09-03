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
pub struct HttpRequest {
    /// HTTP method (`GET`, `POST`, `DELETE`).
    pub method: &'static str,
    /// Absolute URL.
    pub url: String,
    /// Request headers. A `BTreeMap` so the order is deterministic — Route 53's
    /// SigV4 canonical request depends on sorted header names.
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
    fn send<'a>(&'a self, request: HttpRequest) -> BoxFuture<'a, Result<HttpResponse, String>>;
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
            .build()
            .map_err(|e| format!("failed to build the DNS provider HTTP client: {e}"))?;
        Ok(Self { client })
    }
}

impl HttpTransport for ReqwestTransport {
    fn send<'a>(&'a self, request: HttpRequest) -> BoxFuture<'a, Result<HttpResponse, String>> {
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
            let body = response
                .text()
                .await
                .map_err(|e| format!("could not read the DNS provider response body: {e}"))?;
            Ok(HttpResponse { status, body })
        })
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
