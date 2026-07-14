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
//! Each candidate's effective q-value is drawn from the most specific media
//! range that names it, following RFC 7231 §5.3.2 precedence
//! `type/subtype` > `type/*` > `*/*`: `text/html` is covered by
//! `text/html.or(text/*).or(*/*)` and JSON by
//! `application/json.or(application/*).or(*/*)`. The higher effective q wins; on
//! a tie the earlier list entry wins.
//!
//! `application/problem+json` is deliberately **not** part of the JSON tier: it
//! is an error-path (Problem Details) signal, not a success representation, so
//! advertising it alone does not count as accepting the `application/json`
//! success body. A request whose `Accept` names only `application/problem+json`
//! leaves both HTML and JSON unmentioned and falls back to the configured
//! default, exactly like any other unhandled type (e.g. `application/xml`).
//!
//! So `Accept: text/html;q=0.1, */*;q=1`
//! serves **JSON** — `text/html` is explicitly demoted to `q=0.1` while
//! `*/*;q=1` lifts JSON to `q=1` — rather than being fooled by the bare presence
//! of `text/html`. Likewise `Accept: text/*;q=0, */*;q=1` serves **JSON**: the
//! `text/*` range forbids every text format, so only JSON (covered by `*/*`)
//! remains.
//!
//! ## `q=0` exclusions and `406 Not Acceptable`
//!
//! A media range listed with `q=0` means "**not acceptable**" (RFC 7231
//! §5.3.1), not merely "less preferred". A format whose effective q is `0`
//! (because it, or the most specific range covering it — `text/*` /
//! `application/*` or the bare `*/*` — was explicitly demoted to `q=0`) is
//! *forbidden*: it is never served, not via a wildcard and not via the
//! configured default. A more specific range overrides a broader one either way,
//! so `text/html;q=0, text/*;q=1` still forbids HTML while `text/*;q=0, */*;q=1`
//! forbids every text format. So `Accept: text/html;q=0, */*;q=1` serves **JSON**
//! (HTML is forbidden; `*/*` still covers JSON), and `application/json;q=0`
//! serves **HTML** even under `default_format(Format::Json)` (the default may
//! not resurrect a forbidden format). When *both* HTML and JSON are forbidden
//! (e.g. `text/html;q=0, application/json;q=0`, or a bare `*/*;q=0`) the
//! responder answers **`406 Not Acceptable`** with a short plain-text body.
//! Merely *unlisted* types are not exclusions — an `Accept` naming only, say,
//! `application/xml` leaves both HTML and JSON unmentioned and falls back to the
//! default, never a 406.
//!
//! When the client expresses no concrete preference — a missing/empty `Accept`,
//! a bare `*/*`, or a wildcard-only tie where neither side is named directly —
//! the default is [`Format::Html`] (browser-first); override it with
//! [`Negotiate::default_format`]. Responses carry `Vary: Accept` so shared
//! caches key the two representations separately — including the `406` arm.

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
    /// A convenience view over [`Negotiate::resolve`] that collapses the
    /// not-acceptable case onto the configured `default`: it answers the
    /// question "which representation would be served if one *must* be" and so
    /// cannot express a `406`. Prefer [`Negotiate::respond`] for the full
    /// behaviour — only it emits `406 Not Acceptable` when both formats are
    /// forbidden.
    ///
    /// Each candidate's effective quality is drawn from the most specific media
    /// range that names it — `text/html.or(text/*).or(*/*)` for HTML and
    /// `application/json.or(application/*).or(*/*)` for JSON (RFC 7231 §5.3.2
    /// precedence) — so a high-q wildcard can outrank an explicitly demoted
    /// concrete type. A format whose effective q is `0` is forbidden and
    /// never chosen. The higher positive effective q wins; on a tie the earlier
    /// list entry wins. When neither side is named directly (both effective
    /// values come from the same `*/*` entry, or there is no `Accept` at all)
    /// there is no real preference, so the configured `default` applies.
    #[must_use]
    pub fn format(&self) -> Format {
        match self.resolve() {
            Resolution::Html => Format::Html,
            Resolution::Json => Format::Json,
            // Both formats are forbidden; there is no acceptable representation.
            // Collapse onto the default for this lossy view — `respond` emits a
            // real `406` for this case instead.
            Resolution::NotAcceptable => self.default,
        }
    }

    /// Resolve the request's `Accept` header into a concrete decision, honouring
    /// `q=0` exclusions and reporting [`Resolution::NotAcceptable`] when every
    /// format the handler can produce has been explicitly forbidden.
    ///
    /// For each format `F` in {HTML, JSON} the *effective* quality `q_F` is drawn
    /// from the most specific media range that names it (RFC 7231 §5.3.2):
    /// `F`'s own explicit max-q if `F` was listed at all (**including `q=0`**),
    /// else its subtype wildcard (`text/*` for HTML, `application/*` for JSON) if
    /// that was listed (including `q=0`), else the `*/*` wildcard's explicit max-q
    /// if `*/*` was listed (including `q=0`), else `None` (unmentioned). A more
    /// specific range with `q=0` therefore forbids `F` even when a broader range
    /// below it is permissive. Then:
    ///
    /// * `q_F == Some(0.0)` → `F` is **forbidden** (never served — not via the
    ///   wildcard, not via the default).
    /// * Among non-forbidden formats, one with `q_F == Some(>0)` is a *positive
    ///   candidate*; the higher positive q wins, ties broken by earlier list
    ///   index. A tie at the *same* index means both derive from one `*/*` entry
    ///   (no concrete preference) → the `default`, restricted to non-forbidden
    ///   formats.
    /// * If neither is positive but at least one non-forbidden format is
    ///   unmentioned → the `default`, falling back to the other non-forbidden
    ///   format if the default itself is forbidden.
    /// * If **both** formats are forbidden → [`Resolution::NotAcceptable`].
    fn resolve(&self) -> Resolution {
        // Media-range precedence per RFC 7231 §5.3.2: `type/subtype` beats
        // `type/*` beats `*/*`. `.or()` chaining picks the most-specific slot
        // that is present, and because a present slot short-circuits even when
        // it is `Some(0.0)`, a more-specific `q=0` exclusion (e.g. `text/html;q=0`
        // or `text/*;q=0`) still forbids the format regardless of a permissive
        // less-specific range below it.
        let html_eff = self
            .qualities
            .html
            .or(self.qualities.text_star)
            .or(self.qualities.wildcard);
        let json_eff = self
            .qualities
            .json
            .or(self.qualities.application_star)
            .or(self.qualities.wildcard);

        let html_forbidden = matches!(html_eff, Some((q, _)) if q <= 0.0);
        let json_forbidden = matches!(json_eff, Some((q, _)) if q <= 0.0);

        // No representation is acceptable to the client.
        if html_forbidden && json_forbidden {
            return Resolution::NotAcceptable;
        }

        // Positive candidates: listed (directly or via `*/*`) with q > 0.
        let html_pos = html_eff.filter(|&(q, _)| q > 0.0);
        let json_pos = json_eff.filter(|&(q, _)| q > 0.0);

        match (html_pos, json_pos) {
            (Some((hq, hidx)), Some((jq, jidx))) => {
                if (hq - jq).abs() < f32::EPSILON {
                    // Equal effective q: earlier list entry wins. Equal index
                    // means both sides resolved to the *same* `*/*` entry, i.e.
                    // no concrete preference — fall back to the default.
                    match hidx.cmp(&jidx) {
                        std::cmp::Ordering::Less => Resolution::Html,
                        std::cmp::Ordering::Greater => Resolution::Json,
                        std::cmp::Ordering::Equal => {
                            self.default_resolution(html_forbidden, json_forbidden)
                        }
                    }
                } else if hq > jq {
                    Resolution::Html
                } else {
                    Resolution::Json
                }
            }
            // The other side is forbidden or unmentioned; the positive one wins.
            (Some(_), None) => Resolution::Html,
            (None, Some(_)) => Resolution::Json,
            // Neither is a positive candidate, but they are not both forbidden,
            // so at least one is non-forbidden and unmentioned: use the default.
            (None, None) => self.default_resolution(html_forbidden, json_forbidden),
        }
    }

    /// The configured `default` as a [`Resolution`], but never a forbidden
    /// format: if the default itself is forbidden, serve the other side (which
    /// callers guarantee is non-forbidden in every context this is reached).
    const fn default_resolution(&self, html_forbidden: bool, json_forbidden: bool) -> Resolution {
        match self.default {
            Format::Html if html_forbidden => Resolution::Json,
            Format::Html => Resolution::Html,
            Format::Json if json_forbidden => Resolution::Html,
            Format::Json => Resolution::Json,
        }
    }

    /// Serve `html` to browser clients and `json` to API clients.
    ///
    /// The `html` closure runs only when HTML is the chosen representation, so
    /// the markup is never rendered for an API response. When the client has
    /// forbidden every representation the handler can produce (via `q=0`), the
    /// response is `406 Not Acceptable` and neither branch runs.
    #[must_use]
    pub fn respond<F, J>(self, html: F, json: J) -> Negotiated<F, J>
    where
        F: FnOnce() -> maud::Markup,
        J: serde::Serialize,
    {
        Negotiated {
            resolution: self.resolve(),
            html,
            json,
        }
    }
}

/// A resolved content-negotiation decision for the [`Negotiate`] responder.
///
/// Unlike [`Format`], this can express that the client forbade every
/// representation the handler can produce, which the responder answers with
/// `406 Not Acceptable`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Resolution {
    /// Serve the HTML representation.
    Html,
    /// Serve the JSON representation.
    Json,
    /// Every producible format was forbidden (`q=0`) — answer `406`.
    NotAcceptable,
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
/// negotiated [`Resolution`], answers `406 Not Acceptable` when the client
/// forbade every producible representation, and always appends `Vary: Accept`.
pub struct Negotiated<F, J> {
    resolution: Resolution,
    html: F,
    json: J,
}

impl<F, J> IntoResponse for Negotiated<F, J>
where
    F: FnOnce() -> maud::Markup,
    J: serde::Serialize,
{
    fn into_response(self) -> Response {
        let mut response = match self.resolution {
            Resolution::Html => (self.html)().into_response(),
            Resolution::Json => axum::Json(self.json).into_response(),
            Resolution::NotAcceptable => (
                http::StatusCode::NOT_ACCEPTABLE,
                "406 Not Acceptable: no acceptable representation for this resource",
            )
                .into_response(),
        };
        // Append (never insert) so any existing Vary values are preserved. This
        // is done on every arm, including the `406`, so shared caches still key
        // the negotiated representations separately.
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

    // ── Type-wildcard precedence (RFC 7231 §5.3.2): type/* between concrete and */* ──

    #[test]
    fn text_star_forbidden_falls_through_to_wildcard_json() {
        // `text/*;q=0` forbids every text format (including text/html); `*/*;q=1`
        // still covers JSON, so JSON is served rather than HTML resurfacing.
        assert_eq!(
            negotiate(Some("text/*;q=0, */*;q=1")).format(),
            Format::Json,
        );
    }

    #[test]
    fn application_star_forbidden_not_resurrected_by_json_default() {
        // `application/*;q=0` forbids JSON via the subtype wildcard; even a JSON
        // default must not serve it, so HTML is served instead.
        assert_eq!(
            negotiate(Some("application/*;q=0"))
                .default_format(Format::Json)
                .format(),
            Format::Html,
        );
    }

    #[test]
    fn text_star_positive_serves_html() {
        // `text/*;q=1` lifts HTML (via the subtype wildcard) with nothing to beat
        // it, so HTML wins.
        assert_eq!(negotiate(Some("text/*;q=1")).format(), Format::Html);
    }

    #[test]
    fn application_star_positive_serves_json() {
        // `application/*;q=1` lifts JSON (via the subtype wildcard) with nothing
        // to beat it, so JSON wins.
        assert_eq!(negotiate(Some("application/*;q=1")).format(), Format::Json);
    }

    #[test]
    fn concrete_type_beats_type_wildcard_forbidding_html() {
        // Concrete `text/html;q=0` is more specific than `text/*;q=1`, so HTML is
        // forbidden; JSON is unmentioned and non-forbidden, so it is served.
        assert_eq!(
            negotiate(Some("text/html;q=0, text/*;q=1")).resolve(),
            Resolution::Json,
        );
        assert_eq!(
            negotiate(Some("text/html;q=0, text/*;q=1")).format(),
            Format::Json,
        );
    }

    // ── `q=0` exclusions (RFC 7231 §5.3.1) and `406 Not Acceptable` ──────────

    #[test]
    fn forbidden_html_falls_through_to_wildcard_json() {
        // `text/html;q=0` forbids HTML; `*/*;q=1` still covers JSON, so JSON is
        // served rather than the default resurrecting the rejected HTML.
        assert_eq!(
            negotiate(Some("text/html;q=0, */*;q=1")).resolve(),
            Resolution::Json,
        );
    }

    #[test]
    fn forbidden_json_not_resurrected_by_default() {
        // JSON is explicitly forbidden; even a JSON default must not serve it,
        // so the non-forbidden HTML is served instead.
        assert_eq!(
            negotiate(Some("application/json;q=0"))
                .default_format(Format::Json)
                .resolve(),
            Resolution::Html,
        );
    }

    #[test]
    fn both_formats_forbidden_is_not_acceptable() {
        assert_eq!(
            negotiate(Some("text/html;q=0, application/json;q=0")).resolve(),
            Resolution::NotAcceptable,
        );
    }

    #[test]
    fn wildcard_forbidden_is_not_acceptable() {
        // A bare `*/*;q=0` forbids every representation the handler can produce.
        assert_eq!(
            negotiate(Some("*/*;q=0")).resolve(),
            Resolution::NotAcceptable
        );
        // The JSON default cannot rescue it either.
        assert_eq!(
            negotiate(Some("*/*;q=0"))
                .default_format(Format::Json)
                .resolve(),
            Resolution::NotAcceptable,
        );
    }

    #[test]
    fn unlisted_type_is_not_an_exclusion() {
        // Naming only an unrelated type leaves HTML and JSON unmentioned (not
        // forbidden): fall back to the default, never a 406.
        assert_eq!(
            negotiate(Some("application/xml")).resolve(),
            Resolution::Html,
        );
        assert_eq!(
            negotiate(Some("application/xml"))
                .default_format(Format::Json)
                .resolve(),
            Resolution::Json,
        );
    }

    #[test]
    fn demoted_but_positive_html_still_loses_to_wildcard() {
        // Regression: `q=0.1` is a demotion, not an exclusion, and JSON (lifted
        // by `*/*;q=1`) still wins on effective q.
        assert_eq!(
            negotiate(Some("text/html;q=0.1, */*;q=1")).resolve(),
            Resolution::Json,
        );
    }

    // ── `application/problem+json` is an error-path signal, not success JSON ──

    #[test]
    fn problem_json_alone_uses_html_default() {
        // `application/problem+json` is an error-path (Problem Details) signal,
        // not the success `application/json` body. Alone it leaves both HTML and
        // JSON unmentioned, so the HTML default applies — it must NOT select JSON.
        assert_eq!(
            negotiate(Some("application/problem+json")).resolve(),
            Resolution::Html,
        );
        assert_eq!(
            negotiate(Some("application/problem+json")).format(),
            Format::Html,
        );
    }

    #[test]
    fn problem_json_alone_uses_json_default_via_default_not_match() {
        // With a JSON default, problem+json alone still falls through to the
        // default (both formats unmentioned) — JSON is served *because it is the
        // default*, not because problem+json matched the success JSON slot.
        assert_eq!(
            negotiate(Some("application/problem+json"))
                .default_format(Format::Json)
                .resolve(),
            Resolution::Json,
        );
        assert_eq!(
            negotiate(Some("application/problem+json"))
                .default_format(Format::Json)
                .format(),
            Format::Json,
        );
    }

    #[test]
    fn plain_json_serves_json() {
        // Regression: the success `application/json` type still negotiates JSON.
        assert_eq!(
            negotiate(Some("application/json")).resolve(),
            Resolution::Json,
        );
        assert_eq!(negotiate(Some("application/json")).format(), Format::Json);
    }

    #[test]
    fn json_and_problem_json_serves_json() {
        // `application/json` is present (alongside the problem+json error
        // signal), so the success JSON body is negotiated.
        assert_eq!(
            negotiate(Some("application/json, application/problem+json")).resolve(),
            Resolution::Json,
        );
        assert_eq!(
            negotiate(Some("application/json, application/problem+json")).format(),
            Format::Json,
        );
    }

    #[test]
    fn problem_json_with_html_serves_html() {
        // problem+json does not lift the JSON tier, and text/html is present, so
        // HTML wins outright.
        assert_eq!(
            negotiate(Some("application/problem+json, text/html")).resolve(),
            Resolution::Html,
        );
        assert_eq!(
            negotiate(Some("application/problem+json, text/html")).format(),
            Format::Html,
        );
    }

    #[test]
    fn resolve_matches_format_for_non_forbidden_cases() {
        // The lossy `format()` view agrees with `resolve()` whenever a
        // representation is actually acceptable.
        for accept in [
            Some("text/html"),
            Some("application/json"),
            Some("application/json;q=0.9, text/html;q=1.0"),
            Some("text/html;q=1, */*;q=1"),
            Some("*/*"),
            None,
        ] {
            let n = negotiate(accept);
            let expected = match n.resolve() {
                Resolution::Html => Format::Html,
                Resolution::Json => Format::Json,
                Resolution::NotAcceptable => unreachable!("no forbidden case here"),
            };
            assert_eq!(n.format(), expected, "accept={accept:?}");
        }
    }
}
