//! Embedded htmx JavaScript.
//!
//! htmx is embedded directly in the Autumn binary via [`include_bytes!`]
//! and served at [`HTMX_JS_PATH`]. A small CSRF helper is also served at
//! [`HTMX_CSRF_JS_PATH`] so htmx forms can work with Autumn's default
//! `script-src 'self'` Content Security Policy. No CDN, no npm, no build step
//! required.
//!
//! The framework automatically mounts a route handler that serves this
//! file with immutable caching headers. Reference it in your HTML
//! templates:
//!
//! ```html
//! <script src="/static/js/htmx.min.js"></script>
//! <script src="/static/js/autumn-htmx-csrf.js"></script>
//! ```

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use http::header::{HeaderName, HeaderValue};
use std::convert::Infallible;

/// htmx 2.x minified JavaScript, embedded at compile time.
///
/// This is the raw byte content of the minified htmx library. It is
/// served automatically by the framework at `/static/js/htmx.min.js`
/// with `Cache-Control: public, max-age=31536000, immutable`.
pub const HTMX_JS: &[u8] = include_bytes!("../vendor/htmx.min.js");

/// Same-origin path where Autumn serves embedded htmx.
pub const HTMX_JS_PATH: &str = "/static/js/htmx.min.js";

/// htmx SSE extension JavaScript, embedded at compile time.
pub const HTMX_SSE_JS: &[u8] = include_bytes!("../vendor/sse.js");

/// Same-origin path where Autumn serves embedded htmx SSE extension.
pub const HTMX_SSE_JS_PATH: &str = "/static/js/sse.js";

/// Autumn widget runtime JavaScript, embedded at compile time.
///
/// Provides CSP-compatible event-listener wiring for built-in widgets
/// (autocomplete selection, min-length enforcement). Served automatically
/// at [`AUTUMN_WIDGETS_JS_PATH`] with immutable cache headers.
///
/// Reference it once in your layout template:
///
/// ```html
/// <script src="/static/js/autumn-widgets.js" defer></script>
/// ```
pub const AUTUMN_WIDGETS_JS: &[u8] = include_bytes!("../vendor/autumn-widgets.js");

/// Same-origin path where Autumn serves the widget runtime script.
pub const AUTUMN_WIDGETS_JS_PATH: &str = "/static/js/autumn-widgets.js";

/// Same-origin path where Autumn serves the htmx CSRF helper.
///
/// The helper reads a CSRF token from either:
/// - `<meta name="csrf-token" content="...">`
/// - `<meta name="autumn-csrf-token" content="...">`
///
/// The request header defaults to `X-CSRF-Token`; override it with
/// `data-header="..."` on the meta tag when using a custom CSRF header name.
pub const HTMX_CSRF_JS_PATH: &str = "/static/js/autumn-htmx-csrf.js";

/// Idiomorph DOM-morphing library, embedded at compile time.
///
/// Serves at [`IDIOMORPH_JS_PATH`] with immutable caching headers.
/// Enables `hx-swap="morph"` on htmx requests for smooth DOM updates.
///
/// Reference in your layout:
/// ```html
/// <script src="/static/js/idiomorph.min.js"></script>
/// ```
#[cfg(feature = "htmx")]
pub const IDIOMORPH_JS: &[u8] = include_bytes!("../vendor/idiomorph.min.js");

/// Same-origin path where Autumn serves the idiomorph library.
#[cfg(feature = "htmx")]
pub const IDIOMORPH_JS_PATH: &str = "/static/js/idiomorph.min.js";

/// CSP-compatible htmx CSRF helper JavaScript.
///
/// Served as an external same-origin script so applications do not need inline
/// JavaScript under Autumn's default `script-src 'self'` policy.
pub const HTMX_CSRF_JS: &str = r#"(function () {
  document.addEventListener("htmx:configRequest", function (evt) {
    var meta = document.querySelector('meta[name="csrf-token"], meta[name="autumn-csrf-token"]');

    if (!meta || !evt.detail || !evt.detail.headers) {
      return;
    }

    var header = meta.getAttribute("data-header") || "X-CSRF-Token";
    evt.detail.headers[header] = meta.getAttribute("content") || "";
  });
})();
"#;

/// htmx version string for diagnostics and cache busting.
///
/// Corresponds to the version of the embedded htmx JS file.
/// Re-exported at the crate root as [`HTMX_VERSION`].
pub const HTMX_VERSION: &str = "2.0.4";

/// Extractor for htmx request headers.
///
/// Extracts the standard `hx-*` headers sent by htmx requests, enabling
/// conditional rendering (e.g. sending back partials instead of full pages).
///
/// See <https://htmx.org/reference/#request_headers> for more details.
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct HxRequest {
    /// Indicates that the request is via htmx (`HX-Request`)
    pub is_htmx: bool,
    /// The id of the target element if provided (`HX-Target`)
    pub target: Option<String>,
    /// The id of the triggered element if provided (`HX-Trigger`)
    pub trigger: Option<String>,
    /// The name of the triggered element if provided (`HX-Trigger-Name`)
    pub trigger_name: Option<String>,
    /// The current URL of the browser (`HX-Current-URL`)
    pub current_url: Option<String>,
    /// `true` if the request is for history restoration after a miss (`HX-History-Restore-Request`)
    pub history_restore_request: bool,
    /// The user response to an hx-prompt (`HX-Prompt`)
    pub prompt: Option<String>,
    /// `true` if the request is via an element using hx-boost (`HX-Boosted`)
    pub boosted: bool,
}

impl<S> FromRequestParts<S> for HxRequest
where
    S: Send + Sync,
{
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let header_str = |name: &'static str| -> Option<String> {
            parts
                .headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(ToString::to_string)
        };
        let header_bool =
            |name: &'static str| -> bool { parts.headers.get(name).is_some_and(|v| v == "true") };

        Ok(Self {
            is_htmx: header_bool("hx-request"),
            target: header_str("hx-target"),
            trigger: header_str("hx-trigger"),
            trigger_name: header_str("hx-trigger-name"),
            current_url: header_str("hx-current-url"),
            history_restore_request: header_bool("hx-history-restore-request"),
            prompt: header_str("hx-prompt"),
            boosted: header_bool("hx-boosted"),
        })
    }
}

/// Extension trait for adding htmx response headers to any `IntoResponse` type.
///
/// This trait provides a fluent API for controlling htmx behavior from the server.
/// If an invalid header value is provided, it is gracefully ignored.
///
/// See <https://htmx.org/reference/#response_headers> for more details.
pub trait HxResponseExt: IntoResponse + Sized {
    /// Allows you to do a client-side redirect that does not do a full page reload (`HX-Location`).
    fn hx_location(self, url: &str) -> Response {
        append_hx_header(self, "hx-location", url)
    }

    /// Pushes a new URL into the history stack (`HX-Push-Url`).
    fn hx_push_url(self, url: &str) -> Response {
        append_hx_header(self, "hx-push-url", url)
    }

    /// Triggers a client-side redirect (`HX-Redirect`).
    fn hx_redirect(self, url: &str) -> Response {
        append_hx_header(self, "hx-redirect", url)
    }

    /// Tells the client to do a full page refresh (`HX-Refresh`).
    fn hx_refresh(self) -> Response {
        append_hx_header(self, "hx-refresh", "true")
    }

    /// Replaces the current URL in the location bar (`HX-Replace-Url`).
    fn hx_replace_url(self, url: &str) -> Response {
        append_hx_header(self, "hx-replace-url", url)
    }

    /// Specifies how the response will be swapped (`HX-Reswap`).
    fn hx_reswap(self, swap: &str) -> Response {
        append_hx_header(self, "hx-reswap", swap)
    }

    /// Specifies the target element to update (`HX-Retarget`).
    fn hx_retarget(self, target: &str) -> Response {
        append_hx_header(self, "hx-retarget", target)
    }

    /// Triggers client-side events (`HX-Trigger`).
    fn hx_trigger(self, event: &str) -> Response {
        append_hx_header(self, "hx-trigger", event)
    }

    /// Triggers client-side events after the settle step (`HX-Trigger-After-Settle`).
    fn hx_trigger_after_settle(self, event: &str) -> Response {
        append_hx_header(self, "hx-trigger-after-settle", event)
    }

    /// Triggers client-side events after the swap step (`HX-Trigger-After-Swap`).
    fn hx_trigger_after_swap(self, event: &str) -> Response {
        append_hx_header(self, "hx-trigger-after-swap", event)
    }
}

impl<T: IntoResponse> HxResponseExt for T {}

fn append_hx_header<T: IntoResponse>(response: T, name: &'static str, value: &str) -> Response {
    let mut res = response.into_response();
    if let Ok(v) = HeaderValue::from_str(value) {
        res.headers_mut().insert(HeaderName::from_static(name), v);
    }
    res
}

#[cfg(feature = "maud")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OobMethod {
    OuterHTML,
    InnerHTML,
    BeforeBegin,
    AfterBegin,
    BeforeEnd,
    AfterEnd,
    Delete,
}

#[cfg(feature = "maud")]
impl OobMethod {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::OuterHTML => "outerHTML",
            Self::InnerHTML => "innerHTML",
            Self::BeforeBegin => "beforebegin",
            Self::AfterBegin => "afterbegin",
            Self::BeforeEnd => "beforeend",
            Self::AfterEnd => "afterend",
            Self::Delete => "delete",
        }
    }
}

#[cfg(feature = "maud")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OobSwap {
    /// Swap the element outerHTML-style by matching its own ID (`hx-swap-oob="true"`).
    True,
    /// Swap using a specific method, targeting the element matching the fragment's ID.
    OuterHTML,
    InnerHTML,
    BeforeBegin,
    AfterBegin,
    BeforeEnd,
    AfterEnd,
    Delete,
    /// Swap using a specific method targeting a custom CSS selector.
    Target(OobMethod, String),
    /// Already contains `hx-swap-oob` on the root element. Do not wrap in a `<template>` tag.
    Raw,
    /// Custom raw string for the `hx-swap-oob` attribute.
    Custom(String),
}

#[cfg(feature = "maud")]
impl OobSwap {
    #[must_use]
    pub fn format_value<'a>(&'a self, id: &'a str) -> std::borrow::Cow<'a, str> {
        let clean_id = id.strip_prefix('#').unwrap_or(id);
        if clean_id.is_empty() {
            return match self {
                Self::True => std::borrow::Cow::Borrowed("true"),
                Self::OuterHTML => std::borrow::Cow::Borrowed("outerHTML"),
                Self::InnerHTML => std::borrow::Cow::Borrowed("innerHTML"),
                Self::BeforeBegin => std::borrow::Cow::Borrowed("beforebegin"),
                Self::AfterBegin => std::borrow::Cow::Borrowed("afterbegin"),
                Self::BeforeEnd => std::borrow::Cow::Borrowed("beforeend"),
                Self::AfterEnd => std::borrow::Cow::Borrowed("afterend"),
                Self::Delete => std::borrow::Cow::Borrowed("delete"),
                Self::Target(method, _) => std::borrow::Cow::Borrowed(method.as_str()),
                Self::Custom(val) => std::borrow::Cow::Borrowed(val),
                Self::Raw => {
                    unreachable!("Raw strategy should not be formatted into a template wrapper")
                }
            };
        }
        match self {
            Self::True => std::borrow::Cow::Borrowed("true"),
            Self::OuterHTML => std::borrow::Cow::Borrowed("outerHTML"),
            Self::InnerHTML => std::borrow::Cow::Owned(format!("innerHTML:#{clean_id}")),
            Self::BeforeBegin => std::borrow::Cow::Owned(format!("beforebegin:#{clean_id}")),
            Self::AfterBegin => std::borrow::Cow::Owned(format!("afterbegin:#{clean_id}")),
            Self::BeforeEnd => std::borrow::Cow::Owned(format!("beforeend:#{clean_id}")),
            Self::AfterEnd => std::borrow::Cow::Owned(format!("afterend:#{clean_id}")),
            Self::Delete => std::borrow::Cow::Owned(format!("delete:#{clean_id}")),
            Self::Target(method, selector) => {
                std::borrow::Cow::Owned(format!("{}:{}", method.as_str(), selector))
            }
            Self::Custom(val) => std::borrow::Cow::Borrowed(val),
            Self::Raw => {
                unreachable!("Raw strategy should not be formatted into a template wrapper")
            }
        }
    }
}

#[cfg(feature = "maud")]
#[derive(Debug, Clone)]
struct OobFragment {
    id: String,
    strategy: OobSwap,
    markup: maud::Markup,
}

#[cfg(feature = "maud")]
/// A response builder for composing out-of-band multi-region swaps in htmx.
///
/// This builder allows a handler to return a primary HTML fragment plus one or
/// more out-of-band (OOB) fragments that update other disjoint regions of the
/// page.
///
/// # Example
///
/// ```rust
/// use autumn_web::prelude::*;
/// use autumn_web::htmx::{HtmxFragments, OobSwap};
/// use maud::Render;
///
/// let primary = html! { div { "Task created successfully!" } };
/// let flash = html! { div id="flash-message" class="alert alert-success" { "Succesfully saved!" } };
/// let counter = html! { span { "5" } };
///
/// let response = HtmxFragments::new(primary)
///     .oob("flash-message", flash)
///     .oob_with_strategy("task-count", OobSwap::InnerHTML, counter);
///
/// // HtmxFragments implements `IntoResponse` and `maud::Render`
/// let rendered = response.render().into_string();
/// assert!(rendered.contains("<div>Task created successfully!</div>"));
/// assert!(rendered.contains("<template hx-swap-oob=\"true\"><div id=\"flash-message\""));
/// assert!(rendered.contains("<template hx-swap-oob=\"innerHTML:#task-count\"><span>5</span></template>"));
/// ```
#[derive(Debug, Clone)]
pub struct HtmxFragments {
    primary: Option<maud::Markup>,
    oob: Vec<OobFragment>,
}

#[cfg(feature = "maud")]
impl HtmxFragments {
    /// Create a new builder with a primary fragment.
    #[must_use]
    pub const fn new(primary: maud::Markup) -> Self {
        Self {
            primary: Some(primary),
            oob: Vec::new(),
        }
    }

    /// Create a new empty builder (only OOB fragments).
    #[must_use]
    pub const fn oob_only() -> Self {
        Self {
            primary: None,
            oob: Vec::new(),
        }
    }

    /// Attach an out-of-band fragment using the default `OobSwap::True` strategy.
    #[must_use]
    pub fn oob(self, id: impl Into<String>, markup: maud::Markup) -> Self {
        self.oob_with_strategy(id, OobSwap::True, markup)
    }

    /// Attach an out-of-band fragment with a specific swap strategy.
    #[must_use]
    pub fn oob_with_strategy(
        mut self,
        id: impl Into<String>,
        strategy: OobSwap,
        markup: maud::Markup,
    ) -> Self {
        self.oob.push(OobFragment {
            id: id.into(),
            strategy,
            markup,
        });
        self
    }
}

#[cfg(feature = "maud")]
fn escape_attribute(w: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => w.push_str("&amp;"),
            '"' => w.push_str("&quot;"),
            '<' => w.push_str("&lt;"),
            '>' => w.push_str("&gt;"),
            _ => w.push(c),
        }
    }
}

#[cfg(feature = "maud")]
#[must_use]
pub fn escape_attribute_string(s: &str) -> String {
    let mut w = String::with_capacity(s.len() + 10);
    escape_attribute(&mut w, s);
    w
}

#[cfg(feature = "maud")]
#[must_use]
pub fn inject_hx_swap_oob(html: &str, oob_value: &str) -> Option<String> {
    let mut idx = 0;
    while let Some(start_pos) = html[idx..].find('<') {
        let abs_start = idx + start_pos;
        let remaining = &html[abs_start..];
        if remaining.starts_with("<!--") {
            let comment_end = remaining.find("-->")?;
            idx = abs_start + comment_end + 3;
        } else {
            let mut tag_name_end = 0;
            for (char_idx, c) in remaining.char_indices().skip(1) {
                if c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == '>' || c == '/' {
                    tag_name_end = char_idx;
                    break;
                }
            }
            if tag_name_end == 0 {
                return None;
            }
            let insert_pos = abs_start + tag_name_end;
            let escaped_val = escape_attribute_string(oob_value);
            let mut result = String::with_capacity(html.len() + escaped_val.len() + 30);
            result.push_str(&html[..insert_pos]);
            result.push_str(" hx-swap-oob=\"");
            result.push_str(&escaped_val);
            result.push('"');
            result.push_str(&html[insert_pos..]);
            return Some(result);
        }
    }
    None
}

#[cfg(feature = "maud")]
impl maud::Render for HtmxFragments {
    fn render_to(&self, w: &mut String) {
        if let Some(primary) = &self.primary {
            primary.render_to(w);
        }
        for oob in &self.oob {
            let rendered = &oob.markup.0;
            if has_oob_attribute(rendered) || matches!(oob.strategy, OobSwap::Raw) {
                w.push_str(rendered);
            } else {
                let value = oob.strategy.format_value(&oob.id);
                w.push_str("<template hx-swap-oob=\"");
                escape_attribute(w, &value);
                w.push_str("\">");
                w.push_str(rendered);
                w.push_str("</template>");
            }
        }
    }
}

#[cfg(feature = "maud")]
impl IntoResponse for HtmxFragments {
    fn into_response(self) -> Response {
        use maud::Render;

        let mut capacity = 0;
        if let Some(primary) = &self.primary {
            capacity += primary.0.len();
        }
        for oob in &self.oob {
            capacity += oob.markup.0.len() + 64;
        }
        let mut w = String::with_capacity(capacity);

        self.render_to(&mut w);
        axum::response::Html(w).into_response()
    }
}

#[cfg(feature = "maud")]
fn has_oob_attribute(html: &str) -> bool {
    let mut in_tag = false;
    let mut in_quote = None;
    let mut chars = html.char_indices().peekable();

    while let Some((idx, c)) = chars.next() {
        if in_tag {
            if let Some(q) = in_quote {
                if c == q {
                    in_quote = None;
                }
            } else if c == '"' || c == '\'' {
                in_quote = Some(c);
            } else if c == '>' {
                in_tag = false;
            } else {
                let remaining = &html[idx..];
                let match_len = if remaining
                    .get(..11)
                    .is_some_and(|s| s.eq_ignore_ascii_case("hx-swap-oob"))
                {
                    Some(11)
                } else if remaining
                    .get(..16)
                    .is_some_and(|s| s.eq_ignore_ascii_case("data-hx-swap-oob"))
                {
                    Some(16)
                } else {
                    None
                };

                if let Some(len) = match_len {
                    let after = remaining.chars().nth(len);
                    match after {
                        None | Some('=' | ' ' | '\t' | '\n' | '\r' | '>' | '/') => {
                            let is_word_start = if idx == 0 {
                                true
                            } else if let Some(prev_char) = html[..idx].chars().next_back() {
                                prev_char.is_ascii_whitespace()
                                    || prev_char == '/'
                                    || prev_char == '<'
                                    || prev_char == '"'
                                    || prev_char == '\''
                            } else {
                                true
                            };
                            if is_word_start {
                                return true;
                            }
                        }
                        _ => {}
                    }
                }
            }
        } else if c == '<' {
            let remaining = &html[idx..];
            if remaining.starts_with("<!--") {
                while let Some((_, next_c)) = chars.next() {
                    if next_c == '-' {
                        let rem = &html[chars.peek().map_or(html.len(), |&(i, _)| i)..];
                        if rem.starts_with("->") {
                            chars.next();
                            chars.next();
                            break;
                        }
                    }
                }
            } else {
                in_tag = true;
                in_quote = None;
            }
        }
    }
    false
}

/// Extracts the `id` attribute value from the root HTML element in the given HTML string.
///
/// Looks for an `id` attribute within the root start tag (before the first `>`).
/// The attribute name must be preceded by a whitespace boundary.
#[must_use]
pub fn extract_html_id(html: &str) -> Option<String> {
    let start_tag_end = html.find('>')?;
    let start_tag = &html[..start_tag_end];
    let mut id_idx = None;
    let mut search_start = 0;
    while let Some(offset) = start_tag[search_start..].find("id=") {
        let absolute_idx = search_start + offset;
        if absolute_idx > 0 {
            let prev_char = start_tag.as_bytes()[absolute_idx - 1];
            if prev_char == b' ' || prev_char == b'\t' || prev_char == b'\n' || prev_char == b'\r' {
                id_idx = Some(absolute_idx);
                break;
            }
        }
        search_start = absolute_idx + 3;
    }
    let idx = id_idx?;
    let after_id = &start_tag[idx + 3..];
    let mut chars = after_id.chars();
    let quote = chars.next()?;
    if quote == '"' || quote == '\'' {
        let mut val = String::new();
        for c in chars {
            if c == quote {
                break;
            }
            val.push(c);
        }
        Some(val)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;

    #[test]
    #[allow(clippy::const_is_empty)]
    fn htmx_js_is_not_empty() {
        assert!(!HTMX_JS.is_empty(), "htmx.min.js should not be empty");
        assert!(!HTMX_SSE_JS.is_empty(), "sse.js should not be empty");
    }

    #[test]
    fn htmx_js_looks_like_javascript() {
        let start = std::str::from_utf8(&HTMX_JS[..50]).expect("htmx should be valid UTF-8");
        assert!(
            start.contains("htmx") || start.contains("function") || start.contains('('),
            "htmx.min.js doesn't look like JavaScript: {start}"
        );
        let sse_start = std::str::from_utf8(&HTMX_SSE_JS[..50]).expect("sse should be valid UTF-8");
        assert!(
            sse_start.contains("Server")
                || sse_start.contains("function")
                || sse_start.contains('/')
                || sse_start.contains('*'),
            "sse.js doesn't look like JavaScript: {sse_start}"
        );
    }

    #[test]
    fn htmx_version_matches_expected() {
        assert_eq!(HTMX_VERSION, "2.0.4");
    }

    #[test]
    fn htmx_asset_paths_are_same_origin_static_paths() {
        assert_eq!(HTMX_JS_PATH, "/static/js/htmx.min.js");
        assert_eq!(HTMX_CSRF_JS_PATH, "/static/js/autumn-htmx-csrf.js");
        assert_eq!(HTMX_SSE_JS_PATH, "/static/js/sse.js");
    }

    #[test]
    fn htmx_csrf_js_configures_request_header_without_inline_wrapper() {
        assert!(HTMX_CSRF_JS.contains("htmx:configRequest"));
        assert!(HTMX_CSRF_JS.contains("X-CSRF-Token"));
        assert!(HTMX_CSRF_JS.contains("csrf-token"));
        assert!(!HTMX_CSRF_JS.contains("<script"));
    }

    #[test]
    fn widgets_js_wires_nav_bar() {
        let js = std::str::from_utf8(AUTUMN_WIDGETS_JS)
            .expect("autumn-widgets.js should be valid UTF-8");
        assert!(js.contains("data-autumn-nav"), "{js}");
        assert!(js.contains("data-nav-toggle"), "{js}");
        assert!(js.contains("data-nav-menu-toggle"), "{js}");
        assert!(js.contains("aria-expanded"), "{js}");
        assert!(js.contains("Escape"), "{js}");
    }

    #[tokio::test]
    async fn hx_request_extractor_parses_headers() -> Result<(), axum::http::Error> {
        let req = Request::builder()
            .header("hx-request", "true")
            .header("hx-target", "my-div")
            .header("hx-trigger", "btn")
            .header("hx-trigger-name", "btn-name")
            .header("hx-current-url", "http://example.com")
            .header("hx-history-restore-request", "true")
            .header("hx-prompt", "yes")
            .header("hx-boosted", "true")
            .body(())?;
        let (mut parts, ()) = req.into_parts();

        let hx = HxRequest::from_request_parts(&mut parts, &())
            .await
            .expect("infallible");

        assert!(hx.is_htmx);
        assert_eq!(hx.target.as_deref(), Some("my-div"));
        assert_eq!(hx.trigger.as_deref(), Some("btn"));
        assert_eq!(hx.trigger_name.as_deref(), Some("btn-name"));
        assert_eq!(hx.current_url.as_deref(), Some("http://example.com"));
        assert!(hx.history_restore_request);
        assert_eq!(hx.prompt.as_deref(), Some("yes"));
        assert!(hx.boosted);
        Ok(())
    }

    #[tokio::test]
    async fn hx_response_ext_adds_headers() {
        use axum::response::IntoResponse;
        let response = "hello"
            .hx_location("/some-location")
            .hx_push_url("/new-url")
            .hx_redirect("/login")
            .hx_refresh()
            .hx_replace_url("/old-url")
            .hx_reswap("innerHTML")
            .hx_retarget("#target")
            .hx_trigger("my-event")
            .hx_trigger_after_settle("settled-event")
            .hx_trigger_after_swap("swapped-event")
            .into_response();

        let headers = response.headers();
        assert_eq!(headers.get("hx-location").unwrap(), "/some-location");
        assert_eq!(headers.get("hx-push-url").unwrap(), "/new-url");
        assert_eq!(headers.get("hx-redirect").unwrap(), "/login");
        assert_eq!(headers.get("hx-refresh").unwrap(), "true");
        assert_eq!(headers.get("hx-replace-url").unwrap(), "/old-url");
        assert_eq!(headers.get("hx-reswap").unwrap(), "innerHTML");
        assert_eq!(headers.get("hx-retarget").unwrap(), "#target");
        assert_eq!(headers.get("hx-trigger").unwrap(), "my-event");
        assert_eq!(
            headers.get("hx-trigger-after-settle").unwrap(),
            "settled-event"
        );
        assert_eq!(
            headers.get("hx-trigger-after-swap").unwrap(),
            "swapped-event"
        );
    }

    #[tokio::test]
    async fn hx_response_ext_ignores_invalid_header_values() {
        use axum::response::IntoResponse;

        // This value is invalid because it contains a newline character.
        // It should be gracefully ignored by the append_hx_header function.
        let invalid_header_value = "invalid\nvalue";

        let response = "hello"
            .hx_location(invalid_header_value)
            .hx_push_url(invalid_header_value)
            .hx_redirect(invalid_header_value)
            .hx_refresh() // valid by default
            .hx_replace_url(invalid_header_value)
            .hx_reswap(invalid_header_value)
            .hx_retarget(invalid_header_value)
            .hx_trigger(invalid_header_value)
            .hx_trigger_after_settle(invalid_header_value)
            .hx_trigger_after_swap(invalid_header_value)
            .into_response();

        let headers = response.headers();
        assert!(headers.get("hx-location").is_none());
        assert!(headers.get("hx-push-url").is_none());
        assert!(headers.get("hx-redirect").is_none());
        // hx_refresh is always set to "true" internally, so it will be present
        assert_eq!(headers.get("hx-refresh").unwrap(), "true");
        assert!(headers.get("hx-replace-url").is_none());
        assert!(headers.get("hx-reswap").is_none());
        assert!(headers.get("hx-retarget").is_none());
        assert!(headers.get("hx-trigger").is_none());
        assert!(headers.get("hx-trigger-after-settle").is_none());
        assert!(headers.get("hx-trigger-after-swap").is_none());
    }

    #[tokio::test]
    async fn hx_request_extractor_handles_missing_headers() -> Result<(), axum::http::Error> {
        let req = Request::builder().body(())?;
        let (mut parts, ()) = req.into_parts();

        let hx = HxRequest::from_request_parts(&mut parts, &())
            .await
            .expect("infallible");

        assert!(!hx.is_htmx);
        assert_eq!(hx.target, None);
        assert_eq!(hx.trigger, None);
        assert_eq!(hx.trigger_name, None);
        assert_eq!(hx.current_url, None);
        assert!(!hx.history_restore_request);
        assert_eq!(hx.prompt, None);
        assert!(!hx.boosted);
        Ok(())
    }

    #[cfg(feature = "maud")]
    #[tokio::test]
    async fn htmx_fragments_renders_correctly() {
        use axum::response::IntoResponse;

        let primary = maud::html! { div { "primary body" } };
        let oob1 = maud::html! { div id="badge" { "3" } };
        let oob2 = maud::html! { li { "new item" } };

        let response = HtmxFragments::new(primary)
            .oob("badge", oob1)
            .oob_with_strategy("list", OobSwap::BeforeEnd, oob2)
            .into_response();

        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body_bytes.to_vec()).unwrap();

        assert!(
            body_str.contains("<div>primary body</div>"),
            "got: {body_str}"
        );
        assert!(
            body_str
                .contains("<template hx-swap-oob=\"true\"><div id=\"badge\">3</div></template>"),
            "got: {body_str}"
        );
        assert!(
            body_str
                .contains("<template hx-swap-oob=\"beforeend:#list\"><li>new item</li></template>"),
            "got: {body_str}"
        );
    }

    #[cfg(feature = "maud")]
    #[tokio::test]
    async fn htmx_fragments_empty_primary() {
        use axum::response::IntoResponse;

        let oob = maud::html! { div id="badge" { "3" } };

        let response = HtmxFragments::oob_only().oob("badge", oob).into_response();

        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body_bytes.to_vec()).unwrap();

        // Should not contain any stray wrapper or primary body, only the OOB fragment template
        assert_eq!(
            body_str,
            "<template hx-swap-oob=\"true\"><div id=\"badge\">3</div></template>"
        );
    }

    #[cfg(feature = "maud")]
    #[tokio::test]
    async fn htmx_fragments_no_double_wrap() {
        use axum::response::IntoResponse;

        // Already contains hx-swap-oob in markup
        let oob = maud::html! { div id="badge" hx-swap-oob="true" { "3" } };

        let response = HtmxFragments::oob_only().oob("badge", oob).into_response();

        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body_bytes.to_vec()).unwrap();

        // Should not be wrapped in template
        assert_eq!(body_str, "<div id=\"badge\" hx-swap-oob=\"true\">3</div>");
    }

    #[cfg(feature = "maud")]
    #[tokio::test]
    async fn htmx_fragments_composes_with_headers() {
        use axum::response::IntoResponse;

        let primary = maud::html! { div { "ok" } };
        let response = HtmxFragments::new(primary)
            .hx_trigger("custom-event")
            .into_response();

        let headers = response.headers();
        assert_eq!(headers.get("hx-trigger").unwrap(), "custom-event");

        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body_bytes.to_vec()).unwrap();
        assert!(body_str.contains("<div>ok</div>"));
    }

    #[test]
    fn test_has_oob_attribute_detector() {
        // True cases
        assert!(has_oob_attribute("<div hx-swap-oob=\"true\"></div>"));
        assert!(has_oob_attribute("<div data-hx-swap-oob=\"true\"></div>"));
        assert!(has_oob_attribute("<div hx-swap-oob = 'true' ></div>"));
        assert!(has_oob_attribute("<div hx-swap-oob></div>"));
        assert!(has_oob_attribute(
            "<div class=\"x\" hx-swap-oob=\"true\"></div>"
        ));
        assert!(has_oob_attribute(
            "<div hx-swap-oob=\"true\" class=\"x\"></div>"
        ));

        // False cases
        assert!(!has_oob_attribute("<div>Learn hx-swap-oob today</div>"));
        assert!(!has_oob_attribute("<div class=\"hx-swap-oob\"></div>"));
        assert!(!has_oob_attribute(
            "<div id=\"some-hx-swap-oob-element\"></div>"
        ));
        assert!(!has_oob_attribute(
            "<!-- <div hx-swap-oob=\"true\"></div> -->"
        ));
        assert!(!has_oob_attribute(
            "<div class=\"x\">some text hx-swap-oob=\"true\"</div>"
        ));
    }

    #[test]
    fn test_oob_swap_format_value_empty_id() {
        assert_eq!(OobSwap::True.format_value(""), "true");
        assert_eq!(OobSwap::True.format_value("#"), "true");
        assert_eq!(OobSwap::InnerHTML.format_value(""), "innerHTML");
        assert_eq!(OobSwap::InnerHTML.format_value("#"), "innerHTML");
        assert_eq!(OobSwap::BeforeEnd.format_value(""), "beforeend");
        assert_eq!(OobSwap::BeforeEnd.format_value("#"), "beforeend");
        assert_eq!(
            OobSwap::Target(OobMethod::InnerHTML, "#target".to_string()).format_value(""),
            "innerHTML"
        );
        assert_eq!(
            OobSwap::Target(OobMethod::BeforeEnd, "#target".to_string()).format_value("#"),
            "beforeend"
        );

        // Non-empty ID case
        assert_eq!(OobSwap::InnerHTML.format_value("my-id"), "innerHTML:#my-id");
        assert_eq!(
            OobSwap::InnerHTML.format_value("#my-id"),
            "innerHTML:#my-id"
        );
    }

    #[test]
    fn test_inject_hx_swap_oob() {
        assert_eq!(
            inject_hx_swap_oob("<li id=\"1\"></li>", "beforeend:#container"),
            Some("<li hx-swap-oob=\"beforeend:#container\" id=\"1\"></li>".to_string())
        );
        assert_eq!(
            inject_hx_swap_oob("<!-- comment -->\n  <div class=\"foo\"></div>", "outerHTML"),
            Some(
                "<!-- comment -->\n  <div hx-swap-oob=\"outerHTML\" class=\"foo\"></div>"
                    .to_string()
            )
        );
        assert_eq!(inject_hx_swap_oob("Hello world", "true"), None);
    }

    #[cfg(feature = "maud")]
    #[test]
    fn test_inject_on_maud_markup() {
        use maud::html;
        let oob = html! { li id="item-1" { "Item" } };
        let injected = inject_hx_swap_oob(&oob.0, "beforeend:#container");
        assert_eq!(
            injected,
            Some("<li hx-swap-oob=\"beforeend:#container\" id=\"item-1\">Item</li>".to_string())
        );
    }
}
