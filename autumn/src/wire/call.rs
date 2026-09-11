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
        return Err(WireError::Status {
            endpoint,
            status,
            body: truncate_on_boundary(response.text(), MAX_ERROR_BODY),
        });
    }
    decode::<E::Response>(response).map_err(|source| WireError::Transport { endpoint, source })
}

/// Cut `body` to at most `max` bytes, at a character boundary.
///
/// The body is a failing service's own output — `String::from_utf8_lossy` over
/// whatever it sent, so a cut at a fixed byte offset lands mid-character
/// whenever the response is CJK, emoji, or binary turned into U+FFFD runs.
/// `String::truncate` panics there, which would turn a 502 into a panic the
/// upstream service controls.
fn truncate_on_boundary(mut body: String, max: usize) -> String {
    if body.len() <= max {
        return body;
    }
    let cut = (0..=max)
        .rev()
        .find(|&i| body.is_char_boundary(i))
        .unwrap_or(0);
    body.truncate(cut);
    body
}

/// Start the request for this endpoint's declared method.
fn request_builder<E: Endpoint>(http: &Client, url: &str) -> Option<RequestBuilder> {
    let builder = match E::METHOD {
        "GET" => http.get(url),
        "POST" => http.post(url),
        "PUT" => http.put(url),
        "PATCH" => http.patch(url),
        "DELETE" => http.delete(url),
        // Unreachable from generated code: `#[endpoint]` accepts exactly these
        // five route attributes. Kept so adding a sixth there is a 502 with a
        // named cause rather than a silently wrong request.
        _ => return None,
    };
    // A typed RPC call has one destination. Following a `Location` would let a
    // compromised or confused callee steer the caller at an arbitrary host —
    // the SSRF guard cannot help, because an internal service legitimately
    // lives on a private address.
    Some(builder.no_redirect())
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
///
/// A base is an origin plus an optional path prefix. Anything from a `?` or `#`
/// on is dropped rather than concatenated, which would otherwise push the whole
/// route path into the base's query string and send every call to the base path.
fn join_url(base: &str, path: &str) -> String {
    let base = base.split(['?', '#']).next().unwrap_or(base);
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

    #[test]
    fn join_url_drops_a_query_or_fragment_on_the_base() {
        assert_eq!(
            join_url("http://a/?trace=1", "/items/7"),
            "http://a/items/7"
        );
        assert_eq!(join_url("http://a#frag", "/items/7"), "http://a/items/7");
    }

    #[test]
    fn an_error_body_is_cut_at_a_character_boundary() {
        // 'é' is two bytes, so a cut at 512 lands inside it.
        let body = format!("{}\u{e9}tail", "a".repeat(511));
        let cut = truncate_on_boundary(body, 512);
        assert_eq!(cut.len(), 511, "must step back to the boundary");
        assert!(cut.is_char_boundary(cut.len()));
    }

    #[test]
    fn a_short_error_body_is_untouched() {
        assert_eq!(truncate_on_boundary("short".to_owned(), 512), "short");
    }

    #[test]
    fn an_error_body_that_is_one_huge_character_run_cuts_to_empty_rather_than_panicking() {
        let body = "\u{1f600}".repeat(4);
        assert!(truncate_on_boundary(body, 2).is_empty());
    }
}
