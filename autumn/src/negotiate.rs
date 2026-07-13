//! Content-negotiated success responder.
//!
//! One handler can serve HTML to browsers and JSON to API clients from a
//! single source of truth. Declare the [`Negotiate`] extractor, then hand it a
//! Maud closure and a serializable value via [`Negotiate::respond`]:
//!
//! ```rust,ignore
//! use autumn_web::prelude::*;
//!
//! #[get("/widgets/{id}")]
//! async fn show(negotiate: Negotiate) -> impl IntoResponse {
//!     let widget = Widget { id: 1, name: "spanner".into() };
//!     negotiate.respond(
//!         || html! { h1 { (widget.name) } },
//!         widget,
//!     )
//! }
//! ```
//!
//! The client's `Accept` header decides the representation, reusing the crate's
//! one canonical `Accept` parser ([`accept_qualities`]) and resolving over
//! *effective* q-values that honour the `*/*` wildcard per RFC 7231 content
//! negotiation.
//!
//! Each candidate's effective q-value is the better of its own explicit entry
//! and any `*/*` wildcard: `text/html` is covered by `html.or(*/*)` and JSON by
//! `json.or(*/*)`. The higher effective q wins; on a tie the earlier list entry
//! wins. So `Accept: text/html;q=0.1, */*;q=1` serves **JSON** — `text/html` is
//! explicitly demoted to `q=0.1` while `*/*;q=1` lifts JSON to `q=1` — rather
//! than being fooled by the bare presence of `text/html`.
//!
//! When the client expresses no concrete preference — a missing/empty `Accept`,
//! a bare `*/*`, or a wildcard-only tie where neither side is named directly —
//! the default is [`Format::Html`] (browser-first); override it with
//! [`Negotiate::default_format`]. Responses carry `Vary: Accept` so shared
//! caches key the two representations separately.

use std::convert::Infallible;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use http::header::{HeaderValue, VARY};

use crate::middleware::error_page_filter::{AcceptQualities, accept_qualities};

/// The representation a handler can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// An HTML document (`text/html`).
    Html,
    /// A JSON document (`application/json`).
    Json,
}

/// Extractor capturing the request's `Accept` preference so one handler serves
/// HTML or JSON from a single source of truth.
///
/// When the client has no concrete preference (`*/*` or a missing `Accept`),
/// the default is [`Format::Html`] (browser-first); override it with
/// [`Negotiate::default_format`].
#[derive(Debug, Clone, Copy)]
pub struct Negotiate {
    qualities: AcceptQualities,
    default: Format,
}

impl Negotiate {
    /// Override the format used when the client expresses no concrete
    /// preference (`*/*` or a missing `Accept`). Defaults to [`Format::Html`].
    #[must_use]
    pub const fn default_format(mut self, default: Format) -> Self {
        self.default = default;
        self
    }

    /// The resolved representation, chosen by *effective* q-value.
    ///
    /// Each candidate's effective quality folds in the `*/*` wildcard —
    /// `html.or(wildcard)` for HTML and `json.or(wildcard)` for JSON — so a
    /// high-q wildcard can outrank an explicitly demoted concrete type (RFC 7231
    /// content negotiation). The higher effective q wins; on a tie the earlier
    /// list entry wins. When neither side is named directly (both effective
    /// values come from the same `*/*` entry, or there is no `Accept` at all)
    /// there is no real preference, so the configured `default` applies.
    #[must_use]
    pub fn format(&self) -> Format {
        let html_eff = self.qualities.html.or(self.qualities.wildcard);
        let json_eff = self.qualities.json.or(self.qualities.wildcard);

        match (html_eff, json_eff) {
            (Some((hq, hidx)), Some((jq, jidx))) => {
                if (hq - jq).abs() < f32::EPSILON {
                    // Equal effective q: the earlier list entry wins. Equal
                    // index means both sides resolved to the *same* `*/*` entry,
                    // i.e. no concrete preference — fall back to the default.
                    match hidx.cmp(&jidx) {
                        std::cmp::Ordering::Less => Format::Html,
                        std::cmp::Ordering::Greater => Format::Json,
                        std::cmp::Ordering::Equal => self.default,
                    }
                } else if hq > jq {
                    Format::Html
                } else {
                    Format::Json
                }
            }
            (Some(_), None) => Format::Html,
            (None, Some(_)) => Format::Json,
            (None, None) => self.default,
        }
    }

    /// Serve `html` to browser clients and `json` to API clients.
    ///
    /// The `html` closure runs only when HTML is the chosen representation, so
    /// the markup is never rendered for an API response.
    #[must_use]
    pub fn respond<F, J>(self, html: F, json: J) -> Negotiated<F, J>
    where
        F: FnOnce() -> maud::Markup,
        J: serde::Serialize,
    {
        Negotiated {
            format: self.format(),
            html,
            json,
        }
    }
}

impl<S> FromRequestParts<S> for Negotiate
where
    S: Send + Sync,
{
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self {
            qualities: accept_qualities(&parts.headers),
            default: Format::Html,
        })
    }
}

/// The response produced by [`Negotiate::respond`].
///
/// Renders the HTML closure or serializes the JSON value depending on the
/// negotiated [`Format`], and always appends `Vary: Accept`.
pub struct Negotiated<F, J> {
    format: Format,
    html: F,
    json: J,
}

impl<F, J> IntoResponse for Negotiated<F, J>
where
    F: FnOnce() -> maud::Markup,
    J: serde::Serialize,
{
    fn into_response(self) -> Response {
        let mut response = match self.format {
            Format::Html => (self.html)().into_response(),
            Format::Json => axum::Json(self.json).into_response(),
        };
        // Append (never insert) so any existing Vary values are preserved.
        response
            .headers_mut()
            .append(VARY, HeaderValue::from_static("Accept"));
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a [`Negotiate`] as the extractor would, from an optional `Accept`
    /// header value, so `format()` resolution can be asserted directly.
    fn negotiate(accept: Option<&str>) -> Negotiate {
        let mut headers = http::HeaderMap::new();
        if let Some(value) = accept {
            headers.insert(http::header::ACCEPT, HeaderValue::from_str(value).unwrap());
        }
        Negotiate {
            qualities: accept_qualities(&headers),
            default: Format::Html,
        }
    }

    #[test]
    fn wildcard_q_beats_demoted_html() {
        // Codex P2 case: text/html is explicitly demoted to q=0.1 while */*;q=1
        // covers application/json at q=1, so JSON must win.
        assert_eq!(
            negotiate(Some("text/html;q=0.1, */*;q=1")).format(),
            Format::Json,
        );
    }

    #[test]
    fn html_wins_on_higher_q() {
        // Acceptance-criteria case: html has the higher q-value and must win.
        assert_eq!(
            negotiate(Some("application/json;q=0.9, text/html;q=1.0")).format(),
            Format::Html,
        );
    }

    #[test]
    fn explicit_html_ties_wildcard_earlier_index_wins() {
        // text/html and */* tie at q=1; text/html appears first, so it wins.
        assert_eq!(
            negotiate(Some("text/html;q=1, */*;q=1")).format(),
            Format::Html,
        );
    }

    #[test]
    fn explicit_json_beats_demoted_wildcard() {
        // application/json at q=1 outranks the html-covering */*;q=0.1.
        assert_eq!(
            negotiate(Some("application/json, */*;q=0.1")).format(),
            Format::Json,
        );
    }

    #[test]
    fn bare_wildcard_uses_default() {
        // No concrete preference: the configured default applies.
        assert_eq!(negotiate(Some("*/*")).format(), Format::Html);
        assert_eq!(
            negotiate(Some("*/*")).default_format(Format::Json).format(),
            Format::Json,
        );
    }

    #[test]
    fn missing_accept_uses_default() {
        assert_eq!(negotiate(None).format(), Format::Html);
        assert_eq!(
            negotiate(None).default_format(Format::Json).format(),
            Format::Json,
        );
    }
}
