//! Cookie-consent tracking and gating (ePrivacy / GDPR Art. 7).
//!
//! See the [cookie-consent guide](https://github.com/autumn-foundation/autumn/blob/main/docs/guide/cookie-consent.md)
//! for the end-to-end flow, including the withdraw path.
//!
//! Provides the [`Consent`] extractor plus a first-party cookie codec, so an
//! app can read a visitor's cookie-consent choice with a typed helper —
//! `consent.allows("analytics", CURRENT_POLICY_VERSION)` — instead of hand
//! rolling cookie parsing and a bespoke "have they agreed" check.
//!
//! `autumn new` scaffolds a banner (offering "Accept all" / "Reject
//! non-essential") wired automatically into every HTML page via
//! [`inject_consent_banner`], plus `POST /consent/accept` and
//! `POST /consent/reject` routes that call [`accept_all_cookie`] and
//! [`reject_non_essential_cookie`] to record the choice.
//!
//! ## The gate is the enforcement, not just the banner
//!
//! Showing a banner while setting non-essential cookies regardless of the
//! visitor's choice is non-compliant theater. Application code that sets a
//! non-essential cookie or injects a third-party script must check the gate
//! first:
//!
//! ```rust,no_run
//! use autumn_web::consent::Consent;
//!
//! const POLICY_VERSION: u32 = 1;
//!
//! async fn maybe_track(consent: Consent) {
//!     if consent.allows("analytics", POLICY_VERSION) {
//!         // set the analytics cookie / inject the tracking snippet
//!     }
//! }
//! ```
//!
//! With no consent recorded (or a stale `policy_version`, see
//! [`Consent::needs_prompt`]), [`Consent::allows`] returns `false` for every
//! category except `"necessary"`.
//!
//! ## Strictly-necessary cookies are exempt
//!
//! The session cookie ([`crate::session`], default name `autumn.sid`) and the
//! CSRF cookie (`security::csrf`, default name `autumn-csrf`) are
//! never routed through this module's gate at all — [`crate::session::SessionLayer`]
//! and [`crate::security::CsrfLayer`] set them unconditionally,
//! regardless of the visitor's consent choice. Consent is not required for
//! them (ePrivacy's "strictly necessary" exemption), and gating them would
//! break login. `Consent::allows("necessary", _)` always returns `true` so an
//! app can express that exemption explicitly at its own necessary-cookie call
//! sites too, if it wants a single code path either way.
//!
//! ## Re-prompting on a policy change
//!
//! The consent cookie's payload carries the `policy_version` the visitor
//! agreed to, alongside the chosen categories and a timestamp. Bump the
//! app's policy version constant (passed to [`accept_all_cookie`],
//! [`reject_non_essential_cookie`], [`Consent::allows`], and
//! [`Consent::needs_prompt`]) when the cookie policy changes; a cookie
//! recorded under an older version is treated as undecided, so the banner
//! reappears and the gate closes until the visitor re-decides.

use std::convert::Infallible;

use axum::extract::FromRequestParts;
use axum::http::HeaderMap;
use axum::http::header::COOKIE;
use axum::http::request::Parts;

/// Default name of the first-party cookie recording the visitor's consent choice.
pub const CONSENT_COOKIE_NAME: &str = "autumn.consent";

/// Default CSRF cookie name, matching `security::csrf::CsrfConfig`'s own default.
///
/// [`inject_consent_banner`] echoes this cookie's value as the banner forms'
/// `_csrf` field; pass the app's configured name if `security.csrf.cookie_name`
/// has been customized.
#[cfg(feature = "maud")]
pub const DEFAULT_CSRF_COOKIE_NAME: &str = "autumn-csrf";

/// Default CSRF form-field name, matching `security::csrf::CsrfConfig`'s own
/// default.
///
/// [`inject_consent_banner`] uses this as the banner forms' hidden CSRF input
/// name; pass the app's configured name if `security.csrf.form_field` has
/// been customized. A request-time lookup is not an option here: the
/// documented layer stack always places user (`AppBuilder::layer`) middleware
/// like this one outside `CsrfLayer`, so `CsrfLayer`'s `CsrfFormField` request
/// extension does not exist yet when this middleware's request-side code
/// runs, and it is config-derived rather than request-derived anyway.
#[cfg(feature = "maud")]
pub const DEFAULT_CSRF_FORM_FIELD: &str = "_csrf";

/// Category name that is always allowed by [`Consent::allows`].
///
/// Strictly-necessary cookies (session, CSRF) never actually go through this
/// gate — see the module docs — but an app can use this constant at its own
/// necessary-cookie call sites for a single, symmetric code path.
pub const NECESSARY: &str = "necessary";

/// Max age applied to the consent cookie by [`accept_all_cookie`] and
/// [`reject_non_essential_cookie`]: 180 days, within common regulatory
/// guidance (no longer than 12 months) for a consent-decision cookie.
const MAX_AGE_SECS: u64 = 180 * 24 * 60 * 60;

/// The current visitor's recorded (or not-yet-recorded) cookie-consent choice.
///
/// Obtained via the [`FromRequestParts`] extractor — take `consent: Consent`
/// as a handler parameter. Reads directly off the request's `Cookie` header;
/// no middleware or app state is required to use it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Consent {
    categories: Vec<String>,
    policy_version: u32,
    decided_at: Option<String>,
}

impl Consent {
    /// A visitor who has not yet recorded any consent choice.
    #[must_use]
    pub fn undecided() -> Self {
        Self::default()
    }

    /// `true` once the visitor has recorded a choice (accepted or rejected),
    /// regardless of whether that choice is still current — see
    /// [`Consent::needs_prompt`] for the version-aware check.
    #[must_use]
    pub const fn is_decided(&self) -> bool {
        self.decided_at.is_some()
    }

    /// The policy version the visitor's recorded choice was made under.
    /// `0` when undecided.
    #[must_use]
    pub const fn policy_version(&self) -> u32 {
        self.policy_version
    }

    /// RFC 3339 timestamp of when the choice was recorded, if any.
    #[must_use]
    pub fn decided_at(&self) -> Option<&str> {
        self.decided_at.as_deref()
    }

    /// The categories the visitor consented to. Empty for an undecided or
    /// reject-all visitor.
    #[must_use]
    pub fn categories(&self) -> &[String] {
        &self.categories
    }

    /// Whether the given category is allowed to run.
    ///
    /// `"necessary"` (see [`NECESSARY`]) is always allowed. Every other
    /// category requires: a recorded decision, matching `current_policy_version`
    /// (an older-version decision is treated as revoked — see
    /// [`Consent::needs_prompt`]), and the category being in the visitor's
    /// accepted list.
    #[must_use]
    pub fn allows(&self, category: &str, current_policy_version: u32) -> bool {
        if category == NECESSARY {
            return true;
        }
        self.is_decided()
            && self.policy_version == current_policy_version
            && self.categories.iter().any(|c| c == category)
    }

    /// Whether the consent banner should be (re-)shown: no decision recorded
    /// yet, or the recorded decision was made under an older policy version.
    #[must_use]
    pub const fn needs_prompt(&self, current_policy_version: u32) -> bool {
        !self.is_decided() || self.policy_version != current_policy_version
    }

    /// Parse a raw cookie value (already extracted from the `Cookie` header)
    /// into a decided [`Consent`]. Returns `None` for a missing, malformed, or
    /// unparseable value — callers should fall back to [`Consent::undecided`].
    fn from_cookie_value(raw: &str) -> Option<Self> {
        let decoded = percent_decode(raw)?;
        let mut parts = decoded.splitn(3, '|');
        let version: u32 = parts.next()?.parse().ok()?;
        let decided_at = parts.next()?;
        if decided_at.is_empty() || chrono::DateTime::parse_from_rfc3339(decided_at).is_err() {
            return None;
        }
        let categories_field = parts.next().unwrap_or("");
        let categories = if categories_field.is_empty() {
            Vec::new()
        } else {
            categories_field
                .split(',')
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        Some(Self {
            categories,
            policy_version: version,
            decided_at: Some(decided_at.to_owned()),
        })
    }

    /// Build a [`Consent`] directly from request headers, without requiring
    /// the [`FromRequestParts`] extractor machinery. Used by the extractor
    /// impl and by [`inject_consent_banner`] (which needs to inspect consent
    /// before the response exists).
    #[must_use]
    pub fn from_headers(headers: &HeaderMap) -> Self {
        find_cookie(headers, CONSENT_COOKIE_NAME)
            .and_then(|raw| Self::from_cookie_value(&raw))
            .unwrap_or_default()
    }
}

impl<S> FromRequestParts<S> for Consent
where
    S: Send + Sync,
{
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self::from_headers(&parts.headers))
    }
}

/// Find a named cookie's raw value in the `Cookie` request header.
///
/// A malformed pair (no `=`) is skipped rather than aborting the whole scan —
/// a stray valueless cookie sent by some other script on the same site must
/// not make every other cookie on the request invisible. Mirrors
/// [`crate::session::get_cookie`]'s cookie-tossing defense: if `name` appears
/// more than once, that is treated as an attack signal (a legitimate client
/// only ever sends one), and `None` is returned rather than picking either.
pub(crate) fn find_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let mut found = None;
    for cookie_header in headers.get_all(COOKIE) {
        let Ok(cookie_str) = cookie_header.to_str() else {
            continue;
        };
        for pair in cookie_str.split(';') {
            let pair = pair.trim();
            let Some((k, v)) = pair.split_once('=') else {
                continue;
            };
            if k.trim() != name {
                continue;
            }
            if found.is_some() {
                // Multiple cookies with the same name: possible cookie
                // tossing. Reject rather than guess which one is genuine.
                return None;
            }
            found = Some(v.trim().to_owned());
        }
    }
    found
}

// ── Cookie value codec ──────────────────────────────────────────
//
// Payload format (before percent-encoding): `{policy_version}|{decided_at}|{categories}`
// where `categories` is a comma-joined list (empty string when none). The
// whole payload is percent-encoded so `|`, `,`, and RFC 3339's `:`/`+` never
// reach the raw cookie value (mirrors `crate::i18n`'s locale cookie encoding).

fn encode_cookie_value(policy_version: u32, decided_at: &str, categories: &[&str]) -> String {
    let payload = format!("{policy_version}|{decided_at}|{}", categories.join(","));
    percent_encode(&payload)
}

fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if is_unreserved_byte(byte) {
            out.push(char::from(byte));
        } else {
            push_percent_encoded(&mut out, byte);
        }
    }
    out
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = hex_value(*bytes.get(i + 1)?)?;
            let lo = hex_value(*bytes.get(i + 2)?)?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

const fn is_unreserved_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
}

fn push_percent_encoded(output: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    output.push('%');
    output.push(char::from(HEX[(byte >> 4) as usize]));
    output.push(char::from(HEX[(byte & 0x0f) as usize]));
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Build a `Set-Cookie` header value recording that the visitor accepted the
/// given non-essential categories under `policy_version`.
///
/// # Examples
///
/// ```rust
/// use autumn_web::consent::accept_all_cookie;
///
/// let cookie = accept_all_cookie(&["analytics"], 1);
/// assert!(cookie.starts_with("autumn.consent="));
/// assert!(cookie.contains("HttpOnly"));
/// ```
#[must_use]
pub fn accept_all_cookie(categories: &[&str], policy_version: u32) -> String {
    build_consent_cookie(categories, policy_version)
}

/// Build a `Set-Cookie` header value recording that the visitor rejected all
/// non-essential categories under `policy_version`.
///
/// # Examples
///
/// ```rust
/// use autumn_web::consent::reject_non_essential_cookie;
///
/// let cookie = reject_non_essential_cookie(1);
/// assert!(cookie.starts_with("autumn.consent="));
/// ```
#[must_use]
pub fn reject_non_essential_cookie(policy_version: u32) -> String {
    build_consent_cookie(&[], policy_version)
}

fn build_consent_cookie(categories: &[&str], policy_version: u32) -> String {
    let decided_at = chrono::Utc::now().to_rfc3339();
    let value = encode_cookie_value(policy_version, &decided_at, categories);
    format!(
        "{CONSENT_COOKIE_NAME}={value}; Path=/; Max-Age={MAX_AGE_SECS}; HttpOnly; Secure; SameSite=Lax"
    )
}

/// Build a `Set-Cookie` header value that expires the consent cookie
/// immediately, returning the visitor to an undecided state so the banner
/// reappears and they can choose again.
///
/// GDPR Art. 7(3) requires withdrawing consent to be as easy as giving it;
/// wire this to a visible, always-reachable "manage cookie preferences"
/// link (the scaffold adds one to the shared layout's footer) rather than
/// only ever showing the choice once.
///
/// # Examples
///
/// ```rust
/// use autumn_web::consent::expire_consent_cookie;
///
/// let cookie = expire_consent_cookie();
/// assert!(cookie.starts_with("autumn.consent="));
/// assert!(cookie.contains("Max-Age=0"));
/// ```
#[must_use]
pub fn expire_consent_cookie() -> String {
    format!("{CONSENT_COOKIE_NAME}=; Path=/; Max-Age=0; HttpOnly; Secure; SameSite=Lax")
}

/// Validate a redirect target that came from client-controlled input (a
/// `Referer` header, a form field), guarding against an open redirect.
///
/// Returns `path` unchanged when it looks like a safe same-origin relative
/// path — starts with exactly one `/` (not `//`, which browsers treat as
/// scheme-relative and will happily follow off-site), contains no `://`
/// (defense in depth against a scheme smuggled in past the leading slash),
/// carries no `\` — browsers using the WHATWG URL parser treat a backslash
/// exactly like a forward slash when resolving an HTTP(S) URL, so
/// `/\evil.example` would otherwise slip past the `//` check above and still
/// resolve to `https://evil.example/` once a browser follows the `Location`
/// — and carries no ASCII control byte (`\0`-`\x1F`, `\x7F`) at all, not
/// just `\r`/`\n`: the WHATWG URL parser strips ASCII tab and newline bytes
/// from the whole URL before parsing continues, so `/\t/evil.example`
/// resolves to `//evil.example` (scheme-relative) once a browser strips the
/// embedded tab, exactly as if the tab had never been checked for at all.
/// Rejecting the full control-byte range closes that entire class rather
/// than reacting to each individually-discovered stripped byte in turn.
/// Otherwise returns `"/"`.
#[must_use]
pub fn safe_redirect_target(path: &str) -> &str {
    let is_safe = path.starts_with('/')
        && !path.starts_with("//")
        && !path.contains("://")
        && !path.contains('\\')
        && path.bytes().all(|b| !b.is_ascii_control());
    if is_safe { path } else { "/" }
}

/// Derive a safe same-origin redirect target from a `Referer` header value,
/// falling back to `/` when the header is absent, unparseable, or the value
/// has no path at all.
///
/// Used by the scaffolded `/consent/accept` and `/consent/reject` routes so
/// recording a choice returns the visitor to the page they were on instead of
/// always bouncing to the homepage. The scheme and host are discarded
/// entirely (not merely checked) before the result ever reaches
/// [`safe_redirect_target`], so this can never redirect off-site regardless
/// of what a forged `Referer` claims.
#[must_use]
pub fn redirect_target_from_referer(referer: Option<&str>) -> String {
    let Some(value) = referer else {
        return "/".to_owned();
    };
    // `split_once` (not `split(...).nth(1)`): a `split` iterates every
    // occurrence of the delimiter, so a Referer whose path/query legitimately
    // contains a second `://` (e.g. `?source=https://vendor.example/x`) would
    // have `.nth(1)` land on the fragment between the FIRST and SECOND
    // delimiter instead of everything after the first — truncating the
    // target partway through an embedded URL. `split_once` only ever
    // recognizes the first `://`, so the rest of the string (including any
    // further `://` occurrences) survives intact as ordinary path content.
    let after_scheme = value.split_once("://").map_or(value, |(_, rest)| rest);
    let path = after_scheme.find('/').map_or("/", |i| &after_scheme[i..]);
    safe_redirect_target(path).to_owned()
}

#[cfg(feature = "maud")]
mod banner;
#[cfg(feature = "maud")]
pub use banner::{consent_banner_markup, inject_consent_banner};

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with_cookie(raw: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(COOKIE, HeaderValue::from_str(raw).unwrap());
        headers
    }

    // ── Consent::undecided / defaults ──────────────────────────────

    #[test]
    fn undecided_is_not_decided() {
        let consent = Consent::undecided();
        assert!(!consent.is_decided());
        assert_eq!(consent.policy_version(), 0);
        assert_eq!(consent.decided_at(), None);
        assert!(consent.categories().is_empty());
    }

    #[test]
    fn from_headers_with_no_cookie_header_is_undecided() {
        let consent = Consent::from_headers(&HeaderMap::new());
        assert!(!consent.is_decided());
    }

    #[test]
    fn from_headers_with_unrelated_cookies_is_undecided() {
        let headers = headers_with_cookie("autumn.sid=abc123; theme=dark");
        let consent = Consent::from_headers(&headers);
        assert!(!consent.is_decided());
    }

    // A stray valueless cookie pair (no `=`) sent by some other script on the
    // same site must not blind the parser to every cookie after it.
    #[test]
    fn malformed_pair_before_target_cookie_does_not_hide_it() {
        let headers = headers_with_cookie("junk; autumn.sid=abc123");
        assert_eq!(find_cookie(&headers, "autumn.sid"), Some("abc123".into()));
    }

    #[test]
    fn duplicate_consent_cookie_is_rejected_as_possible_tossing() {
        let headers = headers_with_cookie("autumn.consent=aaa; autumn.consent=bbb");
        assert_eq!(find_cookie(&headers, "autumn.consent"), None);
        let consent = Consent::from_headers(&headers);
        assert!(
            !consent.is_decided(),
            "ambiguous duplicate cookie must not be trusted"
        );
    }

    // ── necessary category is always allowed ───────────────────────

    #[test]
    fn necessary_category_always_allowed_even_when_undecided() {
        let consent = Consent::undecided();
        assert!(consent.allows(NECESSARY, 1));
    }

    #[test]
    fn non_necessary_category_denied_when_undecided() {
        let consent = Consent::undecided();
        assert!(!consent.allows("analytics", 1));
        assert!(!consent.allows("marketing", 1));
    }

    // ── accept-all round trip ───────────────────────────────────────

    #[test]
    fn accept_all_cookie_round_trips_and_allows_category() {
        let set_cookie = accept_all_cookie(&["analytics", "marketing"], 1);
        let raw_value = set_cookie
            .split(';')
            .next()
            .unwrap()
            .strip_prefix("autumn.consent=")
            .unwrap();
        let headers = headers_with_cookie(&format!("autumn.consent={raw_value}"));
        let consent = Consent::from_headers(&headers);

        assert!(consent.is_decided());
        assert_eq!(consent.policy_version(), 1);
        assert!(consent.allows("analytics", 1));
        assert!(consent.allows("marketing", 1));
        assert!(consent.allows(NECESSARY, 1));
        assert!(!consent.allows("unrelated-category", 1));
        assert!(consent.decided_at().is_some());
    }

    #[test]
    fn accept_all_cookie_has_expected_attributes() {
        let cookie = accept_all_cookie(&["analytics"], 1);
        assert!(cookie.starts_with("autumn.consent="));
        assert!(cookie.contains("Path=/"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("Secure"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(cookie.contains(&format!("Max-Age={MAX_AGE_SECS}")));
    }

    // ── reject-non-essential round trip ─────────────────────────────

    #[test]
    fn reject_non_essential_cookie_round_trips_and_denies_categories() {
        let set_cookie = reject_non_essential_cookie(1);
        let raw_value = set_cookie
            .split(';')
            .next()
            .unwrap()
            .strip_prefix("autumn.consent=")
            .unwrap();
        let headers = headers_with_cookie(&format!("autumn.consent={raw_value}"));
        let consent = Consent::from_headers(&headers);

        assert!(
            consent.is_decided(),
            "reject is still a recorded decision, not undecided"
        );
        assert!(!consent.allows("analytics", 1));
        assert!(consent.allows(NECESSARY, 1), "necessary always allowed");
        assert!(consent.categories().is_empty());
    }

    // ── withdrawal (expire_consent_cookie) ───────────────────────────

    #[test]
    fn expire_consent_cookie_has_zero_max_age() {
        let cookie = expire_consent_cookie();
        assert!(cookie.starts_with("autumn.consent="));
        assert!(cookie.contains("Max-Age=0"));
    }

    #[test]
    fn expiring_a_prior_accept_returns_to_undecided_and_reopens_the_gate() {
        // A visitor previously accepted analytics...
        let accept = accept_all_cookie(&["analytics"], 1);
        let raw_accept = accept
            .split(';')
            .next()
            .unwrap()
            .strip_prefix("autumn.consent=")
            .unwrap();
        let decided = Consent::from_headers(&headers_with_cookie(&format!(
            "autumn.consent={raw_accept}"
        )));
        assert!(decided.allows("analytics", 1));

        // ...then withdraws via "manage cookie preferences": the browser
        // drops the expired cookie, so the next request carries none at all.
        let expired = expire_consent_cookie();
        assert!(
            expired.contains("Max-Age=0"),
            "confirms the cookie a browser would discard"
        );
        let withdrawn = Consent::from_headers(&HeaderMap::new());
        assert!(!withdrawn.is_decided());
        assert!(!withdrawn.allows("analytics", 1));
        assert!(withdrawn.needs_prompt(1), "banner must reappear");
    }

    // ── needs_prompt: undecided and version staleness ───────────────

    #[test]
    fn needs_prompt_true_when_undecided() {
        assert!(Consent::undecided().needs_prompt(1));
    }

    #[test]
    fn needs_prompt_false_when_decided_under_current_version() {
        let headers = decided_headers(&["analytics"], 1);
        let consent = Consent::from_headers(&headers);
        assert!(!consent.needs_prompt(1));
    }

    #[test]
    fn needs_prompt_true_after_policy_version_bump() {
        let headers = decided_headers(&["analytics"], 1);
        let consent = Consent::from_headers(&headers);
        // The app bumped its policy version from 1 to 2.
        assert!(consent.needs_prompt(2));
    }

    #[test]
    fn allows_denies_category_after_policy_version_bump_even_if_previously_accepted() {
        let headers = decided_headers(&["analytics"], 1);
        let consent = Consent::from_headers(&headers);
        assert!(consent.allows("analytics", 1));
        // Re-prompt is not just cosmetic: the gate itself closes under the new version.
        assert!(!consent.allows("analytics", 2));
        // ...but necessary is still exempt regardless of version.
        assert!(consent.allows(NECESSARY, 2));
    }

    fn decided_headers(categories: &[&str], policy_version: u32) -> HeaderMap {
        let set_cookie = accept_all_cookie(categories, policy_version);
        let raw_value = set_cookie
            .split(';')
            .next()
            .unwrap()
            .strip_prefix("autumn.consent=")
            .unwrap();
        headers_with_cookie(&format!("autumn.consent={raw_value}"))
    }

    // ── malformed cookie values are treated as undecided ────────────

    #[test]
    fn malformed_cookie_value_is_undecided_not_a_panic() {
        for bogus in [
            "not-percent-encoded-but-fine-chars",
            "1",
            "1|",
            "%zz",
            "",
            "abc|2024-01-01T00:00:00Z|analytics",
        ] {
            let headers = headers_with_cookie(&format!("autumn.consent={bogus}"));
            let consent = Consent::from_headers(&headers);
            assert!(
                !consent.is_decided(),
                "bogus value {bogus:?} must decode to undecided, not panic"
            );
        }
    }

    #[test]
    fn cookie_with_invalid_timestamp_is_undecided() {
        let value = percent_encode("1|not-a-timestamp|analytics");
        let headers = headers_with_cookie(&format!("autumn.consent={value}"));
        let consent = Consent::from_headers(&headers);
        assert!(!consent.is_decided());
    }

    // ── strictly-necessary cookies are untouched by this module ─────

    #[test]
    fn session_and_csrf_cookies_survive_alongside_an_undecided_consent() {
        // Simulates a request that already carries the session + CSRF cookies
        // (set unconditionally by SessionLayer / CsrfLayer) but has never
        // recorded a consent choice. This module must not need or touch those
        // cookies at all — it only ever reads its own cookie name.
        let headers = headers_with_cookie("autumn.sid=session-value; autumn-csrf=csrf-value");
        let consent = Consent::from_headers(&headers);
        assert!(!consent.is_decided());
        assert!(consent.allows(NECESSARY, 1));
    }

    // ── percent encode/decode round trip ─────────────────────────────

    #[test]
    fn percent_encode_decode_round_trips_reserved_characters() {
        let original = "1|2024-01-01T00:00:00+00:00|analytics,marketing";
        let encoded = percent_encode(original);
        assert!(!encoded.contains('|'), "pipe must be encoded: {encoded}");
        assert!(!encoded.contains(':'), "colon must be encoded: {encoded}");
        assert_eq!(percent_decode(&encoded).unwrap(), original);
    }

    #[test]
    fn percent_decode_rejects_truncated_escape() {
        assert_eq!(percent_decode("%4"), None);
        assert_eq!(percent_decode("%"), None);
    }

    #[test]
    fn percent_decode_rejects_invalid_hex() {
        assert_eq!(percent_decode("%zz"), None);
    }

    // ── safe_redirect_target / redirect_target_from_referer ─────────

    #[test]
    fn safe_redirect_target_allows_plain_relative_path() {
        assert_eq!(safe_redirect_target("/blog/post-1"), "/blog/post-1");
        assert_eq!(safe_redirect_target("/"), "/");
        assert_eq!(safe_redirect_target("/a?b=c"), "/a?b=c");
    }

    #[test]
    fn safe_redirect_target_rejects_scheme_relative_open_redirect() {
        assert_eq!(safe_redirect_target("//evil.example.com"), "/");
    }

    #[test]
    fn safe_redirect_target_rejects_absolute_url() {
        assert_eq!(safe_redirect_target("https://evil.example.com"), "/");
        assert_eq!(safe_redirect_target("javascript://alert(1)"), "/");
    }

    #[test]
    fn safe_redirect_target_rejects_missing_leading_slash() {
        assert_eq!(safe_redirect_target("evil.example.com"), "/");
        assert_eq!(safe_redirect_target(""), "/");
    }

    #[test]
    fn safe_redirect_target_rejects_embedded_crlf() {
        assert_eq!(safe_redirect_target("/x\r\nSet-Cookie: pwned=1"), "/");
    }

    #[test]
    fn safe_redirect_target_rejects_tab_based_scheme_relative_bypass() {
        // The WHATWG URL parser strips ASCII tab/newline bytes from the
        // whole URL before parsing continues, so `/\t/evil.example` becomes
        // `//evil.example` (scheme-relative) once a browser strips the tab.
        assert_eq!(safe_redirect_target("/\t/evil.example"), "/");
    }

    #[test]
    fn safe_redirect_target_rejects_any_ascii_control_byte() {
        // Rejecting the whole control-byte range (not just the specific
        // bytes browsers are known to strip today) closes this class of
        // bypass at once rather than reacting to each one individually.
        assert_eq!(safe_redirect_target("/x\0evil"), "/");
        assert_eq!(safe_redirect_target("/x\x0Bevil"), "/");
        assert_eq!(safe_redirect_target("/x\x7Fevil"), "/");
    }

    #[test]
    fn safe_redirect_target_rejects_backslash_based_scheme_relative_bypass() {
        // Browsers using the WHATWG URL parser treat `\` exactly like `/`
        // when resolving an HTTP(S) URL, so `/\evil.example` would otherwise
        // slip past the plain `//` check and still resolve off-site.
        assert_eq!(safe_redirect_target("/\\evil.example"), "/");
        assert_eq!(safe_redirect_target("/\\\\evil.example"), "/");
        assert_eq!(safe_redirect_target("/ok/path\\evil.example"), "/");
    }

    #[test]
    fn redirect_target_from_referer_extracts_path_and_query() {
        assert_eq!(
            redirect_target_from_referer(Some("https://app.example.com/blog/post-1?x=1")),
            "/blog/post-1?x=1"
        );
    }

    #[test]
    fn redirect_target_from_referer_falls_back_to_root_when_absent() {
        assert_eq!(redirect_target_from_referer(None), "/");
    }

    #[test]
    fn redirect_target_from_referer_discards_scheme_and_host_entirely() {
        // Even a maliciously-crafted Referer cannot smuggle an off-site
        // target through, because the scheme+host are split off and
        // discarded before any safety check runs.
        assert_eq!(
            redirect_target_from_referer(Some("https://good.example.com//evil.example.com")),
            "/"
        );
    }

    #[test]
    fn redirect_target_from_referer_falls_back_when_no_path_present() {
        assert_eq!(
            redirect_target_from_referer(Some("https://example.com")),
            "/"
        );
    }

    #[test]
    fn redirect_target_from_referer_does_not_silently_accept_a_scheme_truncated_target() {
        // A naive `split("://").nth(1)` would stop at the SECOND `://`
        // occurrence (the one inside the query value) rather than only the
        // first, silently truncating the target to `/docs?source=https` —
        // which, having lost its own embedded `://`, would then sail right
        // past `safe_redirect_target`'s unrelated "no `://` anywhere" guard
        // and get silently accepted as a corrupted redirect target.
        // `split_once` preserves the whole thing instead, so the candidate
        // downstream still legitimately contains `://` and is correctly
        // rejected by that guard, falling back to the safe "/" rather than
        // redirecting to a mangled URL.
        let result = redirect_target_from_referer(Some(
            "https://app.example.com/docs?source=https://vendor.example.com/x",
        ));
        assert_ne!(
            result, "/docs?source=https",
            "must not silently truncate to (and accept) a corrupted target: {result}"
        );
        assert_eq!(result, "/");
    }
}
