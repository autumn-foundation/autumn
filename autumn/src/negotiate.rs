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
//! one canonical q-value negotiator. When the client expresses no concrete
//! preference (`*/*` or a missing `Accept`), the default is
//! [`Format::Html`] (browser-first); override it with
//! [`Negotiate::default_format`]. Responses carry `Vary: Accept` so shared
//! caches key the two representations separately.

use std::convert::Infallible;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use http::header::{HeaderValue, VARY};

use crate::middleware::error_page_filter::{AcceptPreference, accept_preference};

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
    preference: AcceptPreference,
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

    /// The resolved representation: a direct `text/html` / `application/json`
    /// preference maps straight through, while `*/*` or an unspecified
    /// `Accept` falls back to the configured default.
    #[must_use]
    pub const fn format(&self) -> Format {
        match self.preference {
            AcceptPreference::Html => Format::Html,
            AcceptPreference::Json => Format::Json,
            AcceptPreference::Any | AcceptPreference::Unspecified => self.default,
        }
    }

    /// Serve `html` to browser clients and `json` to API clients.
    ///
    /// The `html` closure runs only when HTML is the chosen representation, so
    /// the markup is never rendered for an API response.
    #[must_use]
    pub const fn respond<F, J>(self, html: F, json: J) -> Negotiated<F, J>
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
            preference: accept_preference(&parts.headers),
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
