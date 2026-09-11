//! The one outbound call a generated client makes.
//!
//! Transport is deliberately fixed for this slice: JSON over HTTP, synchronous
//! request/response, through [`crate::http_client::Client`] so the call keeps
//! trace propagation, retries and `TestApp::http_mock` support.

use serde::de::DeserializeOwned;

use crate::http_client::{Client, ClientError, RequestBuilder};
use crate::wire::Endpoint;

/// Why a typed service call failed.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    /// The request never completed, or the body was not valid JSON.
    #[error("service call to {endpoint} failed: {source}")]
    Transport {
        /// `service.name` of the endpoint called.
        endpoint: &'static str,
        /// The underlying client error.
        #[source]
        source: ClientError,
    },
    /// The service answered with a non-2xx status.
    #[error("service call to {endpoint} returned {status}: {body}")]
    Status {
        /// `service.name` of the endpoint called.
        endpoint: &'static str,
        /// The status code returned.
        status: u16,
        /// The response body, truncated for the message.
        body: String,
    },
    /// The endpoint declares an HTTP method the client cannot issue.
    #[error("service call to {endpoint} declares unsupported method `{method}`")]
    UnsupportedMethod {
        /// `service.name` of the endpoint called.
        endpoint: &'static str,
        /// The method the endpoint declared.
        method: &'static str,
    },
}

impl WireError {
    /// The status a handler returns when `?` propagates this error.
    ///
    /// Always 502: the caller's own request was not at fault — the service it
    /// depends on was unreachable, slow, or answered with something the
    /// contract does not describe. Propagating the upstream status instead
    /// would blame the client for a dependency's 404.
    #[must_use]
    pub const fn status(&self) -> http::StatusCode {
        http::StatusCode::BAD_GATEWAY
    }
}

/// How much of an error body a [`WireError::Status`] carries.
const MAX_ERROR_BODY: usize = 512;

/// Issue one typed call to `endpoint`.
///
/// `path` is [`Endpoint::PATH`] with its `{param}` placeholders already
/// substituted — the generated client does that, so it stays the one place
/// that knows the parameter order.
///
/// # Errors
/// [`WireError`] on transport failure, a non-2xx status, or an undecodable body.
pub async fn call<E: Endpoint>(
    http: &Client,
    base_url: &str,
    path: &str,
    request: &E::Request,
) -> Result<E::Response, WireError> {
    let endpoint = E::NAME;
    let url = join_url(base_url, path);
    let Some(mut builder) = request_builder::<E>(http, &url) else {
        return Err(WireError::UnsupportedMethod {
            endpoint,
            method: E::METHOD,
        });
    };
    if E::HAS_BODY {
        builder = builder.json(request);
    }
    let response = builder
        .send()
        .await
        .map_err(|source| WireError::Transport { endpoint, source })?;
    let status = response.status().as_u16();
    if !response.is_success() {
        let mut body = response.text();
        body.truncate(MAX_ERROR_BODY);
        return Err(WireError::Status {
            endpoint,
            status,
            body,
        });
    }
    decode::<E::Response>(response).map_err(|source| WireError::Transport { endpoint, source })
}

/// Start the request for this endpoint's declared method.
fn request_builder<E: Endpoint>(http: &Client, url: &str) -> Option<RequestBuilder> {
    Some(match E::METHOD {
        "GET" => http.get(url),
        "POST" => http.post(url),
        "PUT" => http.put(url),
        "PATCH" => http.patch(url),
        "DELETE" => http.delete(url),
        _ => return None,
    })
}

/// Decode a success body, treating an empty one as JSON `null`.
///
/// A handler returning `Json<()>` sends no bytes at all through some proxies;
/// `null` is what `()` deserializes from, so an empty 204 still decodes.
fn decode<T: DeserializeOwned>(response: crate::http_client::Response) -> Result<T, ClientError> {
    let bytes = response.bytes();
    if bytes.is_empty() {
        return serde_json::from_slice(b"null").map_err(ClientError::Json);
    }
    serde_json::from_slice(&bytes).map_err(ClientError::Json)
}

/// Join a base URL and a route path with exactly one slash between them.
fn join_url(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    if path.starts_with('/') {
        format!("{base}{path}")
    } else {
        format!("{base}/{path}")
    }
}

#[cfg(test)]
mod tests {
    use super::join_url;

    #[test]
    fn join_url_never_doubles_or_drops_the_slash() {
        assert_eq!(join_url("http://a", "/items"), "http://a/items");
        assert_eq!(join_url("http://a/", "/items"), "http://a/items");
        assert_eq!(join_url("http://a/", "items"), "http://a/items");
        assert_eq!(join_url("http://a/v1/", "/items"), "http://a/v1/items");
    }
}
