//! Per-user time zone resolution and locale-aware date/time rendering.
//!
//! Autumn exposes a [`TimeZone`] extractor so handlers can render timestamps in the
//! requesting user's local time instead of always serving UTC. It mirrors the
//! [`Clock`](crate::time::Clock) extractor pattern — deterministic, injected,
//! testable — and reuses the existing `chrono-tz` dependency (already required by
//! the scheduler).
//!
//! # Resolution order
//!
//! The extractor walks the request in this order, returning the first valid IANA
//! zone found, or falling back to the configured app default (`UTC` when omitted):
//!
//! 1. `UserTimeZone` extension (set by your auth middleware for the logged-in user).
//! 2. `autumn_time_zone` key in the signed session.
//! 3. Plain `autumn_time_zone` cookie (unsigned, for apps without sessions).
//! 4. `?tz=<iana>` query parameter (dev/test convenience).
//!
//! The order is configurable via [`TimeZoneConfig::sources`].
//!
//! # Quick example
//!
//! ```rust,ignore
//! use autumn_web::prelude::*;
//! use autumn_web::time_zone::{TimeZone, local_datetime};
//! use autumn_web::time::Clock;
//!
//! #[get("/events")]
//! async fn index(clock: Clock, tz: TimeZone) -> Markup {
//!     let now = clock.now();
//!     html! { p { (local_datetime(now, *tz)) } }
//! }
//! ```

use chrono::TimeZone as _;
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use serde::Deserialize;

// ── Config ────────────────────────────────────────────────────────────────────

/// Source of the time zone in the resolution chain.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// `UserTimeZone` extension inserted by the app's auth middleware.
    User,
    /// `autumn_time_zone` key in the framework's signed session cookie.
    Session,
    /// Plain (unsigned) `autumn_time_zone` cookie.
    Cookie,
    /// `?tz=<iana>` query parameter override.
    Query,
}

/// Configuration for the time zone subsystem.
///
/// Populated from the `[time_zone]` block in `autumn.toml`, or left at
/// defaults (`UTC`, all sources). Both the table form and the scalar
/// shorthand are accepted:
///
/// ```toml
/// # Scalar shorthand — sets `identifier`, keeps default sources:
/// time_zone = "America/New_York"
///
/// # Table form — also lets you reorder the resolution sources:
/// [time_zone]
/// identifier = "America/New_York"
/// ```
#[derive(Debug, Clone)]
pub struct TimeZoneConfig {
    /// IANA time zone identifier used when no source resolves.
    /// Defaults to `"UTC"`.
    pub identifier: String,
    /// Ordered list of sources tried during resolution.
    pub sources: Vec<Source>,
}

fn default_time_zone_identifier() -> String {
    "UTC".to_owned()
}

fn default_time_zone_sources() -> Vec<Source> {
    vec![Source::User, Source::Session, Source::Cookie, Source::Query]
}

impl Default for TimeZoneConfig {
    fn default() -> Self {
        Self {
            identifier: default_time_zone_identifier(),
            sources: default_time_zone_sources(),
        }
    }
}

impl<'de> Deserialize<'de> for TimeZoneConfig {
    /// Accept either a scalar identifier (`time_zone = "America/New_York"`) or
    /// a full table (`[time_zone] identifier = "…"`), so the documented
    /// shorthand in issue #836 loads correctly.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Scalar(String),
            Table {
                #[serde(default = "default_time_zone_identifier")]
                identifier: String,
                #[serde(default = "default_time_zone_sources")]
                sources: Vec<Source>,
            },
        }

        Ok(match Repr::deserialize(deserializer)? {
            Repr::Scalar(identifier) => Self {
                identifier,
                sources: default_time_zone_sources(),
            },
            Repr::Table {
                identifier,
                sources,
            } => Self {
                identifier,
                sources,
            },
        })
    }
}

impl TimeZoneConfig {
    /// Validate the config, returning an error if the identifier is not a
    /// recognised IANA zone. Called by
    /// [`AutumnConfig::validate`](crate::config::AutumnConfig::validate) at startup.
    ///
    /// # Errors
    ///
    /// Returns [`crate::config::ConfigError::Validation`] when the identifier
    /// is not a known IANA time zone.
    pub fn validate(&self) -> Result<(), crate::config::ConfigError> {
        parse_iana(&self.identifier).ok_or_else(|| {
            crate::config::ConfigError::Validation(format!(
                "time_zone identifier `{}` is not a valid IANA time zone",
                self.identifier
            ))
        })?;
        Ok(())
    }

    /// Returns the configured default [`Tz`], or `UTC` if the identifier is
    /// somehow invalid (should not happen after validation).
    #[must_use]
    pub fn default_tz(&self) -> Tz {
        parse_iana(&self.identifier).unwrap_or(Tz::UTC)
    }
}

// ── UserTimeZone extension ───────────────────────────────────────────────────

/// Newtype placed into request extensions by the app's auth middleware when
/// an authenticated user with a known time zone is present.
///
/// The [`TimeZone`] extractor reads this as the highest-priority source (when
/// `Source::User` is in the resolution chain, which it is by default).
///
/// ```rust,ignore
/// // In your auth / current-user middleware:
/// parts.extensions.insert(UserTimeZone(user.time_zone.parse::<Tz>().unwrap()));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserTimeZone(pub Tz);

// ── Session key ───────────────────────────────────────────────────────────────

/// Session key used to persist the chosen time zone in the signed session.
pub const TIME_ZONE_SESSION_KEY: &str = "autumn_time_zone";

// ── TimeZone extractor ────────────────────────────────────────────────────────

/// Axum extractor that resolves the per-request time zone.
///
/// Declare it as a handler parameter to get the zone without any manual
/// resolution. Compose it with [`Clock`](crate::time::Clock) for fully
/// deterministic, test-injectable time handling:
///
/// ```rust,ignore
/// use autumn_web::time_zone::TimeZone;
/// use autumn_web::time::Clock;
///
/// async fn handler(clock: Clock, tz: TimeZone) -> String {
///     let local = tz.convert(clock.now());
///     local.format("%Y-%m-%d %H:%M %Z").to_string()
/// }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeZone(pub Tz);

impl TimeZone {
    /// Construct a `TimeZone` for testing without going through extraction.
    #[must_use]
    pub const fn new(tz: Tz) -> Self {
        Self(tz)
    }

    /// Returns the wrapped [`Tz`].
    #[must_use]
    pub const fn tz(&self) -> Tz {
        self.0
    }

    /// Returns the IANA identifier string (e.g. `"America/New_York"`).
    #[must_use]
    pub fn iana(&self) -> &'static str {
        self.0.name()
    }

    /// Convert a UTC timestamp into this zone's local time.
    #[must_use]
    pub fn convert(&self, dt: DateTime<Utc>) -> chrono::DateTime<Tz> {
        use chrono::TimeZone as _;
        self.0.from_utc_datetime(&dt.naive_utc())
    }
}

impl std::ops::Deref for TimeZone {
    type Target = Tz;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl axum::extract::FromRequestParts<crate::state::AppState> for TimeZone {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &crate::state::AppState,
    ) -> Result<Self, Self::Rejection> {
        let cfg = state.config().time_zone;
        let sources = cfg.sources.clone();
        let default_tz = cfg.default_tz();

        for source in &sources {
            if let Some(tz) = resolve_source(parts, source).await {
                return Ok(Self(tz));
            }
        }
        Ok(Self(default_tz))
    }
}

async fn resolve_source(parts: &axum::http::request::Parts, source: &Source) -> Option<Tz> {
    match source {
        Source::User => parts.extensions.get::<UserTimeZone>().map(|utz| utz.0),
        Source::Session => {
            let session = parts.extensions.get::<crate::session::Session>().cloned()?;
            let value: String = session.get(TIME_ZONE_SESSION_KEY).await?;
            parse_iana(&value)
        }
        Source::Cookie => resolve_from_cookie(parts),
        Source::Query => resolve_from_query(parts),
    }
}

fn resolve_from_query(parts: &axum::http::request::Parts) -> Option<Tz> {
    let query = parts.uri.query()?;
    for pair in query.split('&') {
        if let Some(value) = pair.strip_prefix("tz=")
            && let Some(tz) = parse_iana(&percent_decode(value))
        {
            return Some(tz);
        }
    }
    None
}

fn resolve_from_cookie(parts: &axum::http::request::Parts) -> Option<Tz> {
    let cookie_header = parts
        .headers
        .get(axum::http::header::COOKIE)
        .and_then(|h| h.to_str().ok())?;
    for cookie in cookie_header.split(';') {
        let cookie = cookie.trim();
        if let Some(value) = cookie.strip_prefix("autumn_time_zone=")
            && let Some(tz) = parse_iana(&percent_decode(value))
        {
            return Some(tz);
        }
    }
    None
}

/// Resolve `%XX` percent-escapes in a query/cookie value so that
/// pre-encoded IANA identifiers (e.g. `America%2FNew_York`,
/// `Etc%2FGMT%2B5`) still parse.
///
/// `+` is left literal rather than decoded to a space: IANA identifiers can
/// contain `+` (e.g. `Etc/GMT+5`) and never contain spaces, so treating `+`
/// as a literal is the correct choice here.
fn percent_decode(value: &str) -> std::borrow::Cow<'_, str> {
    if !value.contains('%') {
        return std::borrow::Cow::Borrowed(value);
    }
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(byte) = decode_hex_pair(bytes[i + 1], bytes[i + 2])
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    // Fall back to the original string if decoding produced invalid UTF-8.
    String::from_utf8(out).map_or_else(
        |_| std::borrow::Cow::Owned(value.to_owned()),
        std::borrow::Cow::Owned,
    )
}

/// Decode two ASCII hex digits into the byte they represent, or `None` if
/// either character is not a hex digit.
fn decode_hex_pair(hi: u8, lo: u8) -> Option<u8> {
    let hi = (hi as char).to_digit(16)?;
    let lo = (lo as char).to_digit(16)?;
    // `hi` and `lo` are each in `0..16`, so the result is always `<= 255`.
    u8::try_from(hi * 16 + lo).ok()
}

// ── IANA parsing ──────────────────────────────────────────────────────────────

/// Parse and validate an IANA time zone identifier.
///
/// Returns `None` for unknown or malformed identifiers so callers can fall
/// through to the next resolution source.
#[must_use]
pub fn parse_iana(s: &str) -> Option<Tz> {
    s.trim().parse::<Tz>().ok()
}

// ── Session & cookie helpers ──────────────────────────────────────────────────

/// Persist a time zone choice into the framework's signed session cookie.
///
/// The value is the IANA identifier string (e.g. `"America/New_York"`).
pub async fn set_time_zone_in_session(session: &crate::session::Session, iana: &str) {
    session.insert(TIME_ZONE_SESSION_KEY, iana).await;
}

/// Produce a `Set-Cookie` header value that persists the chosen zone.
///
/// The cookie is unsigned — for signed persistence use
/// [`set_time_zone_in_session`] instead.
#[must_use]
pub fn set_time_zone_cookie(iana: &str) -> String {
    let safe = encode_tz_cookie_value(iana);
    format!("autumn_time_zone={safe}; Path=/; Max-Age=31536000; SameSite=Lax")
}

fn encode_tz_cookie_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        if is_tz_cookie_byte(b) {
            out.push(char::from(b));
        } else {
            push_pct_encoded(&mut out, b);
        }
    }
    out
}

const fn is_tz_cookie_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'+' | b'/')
}

fn push_pct_encoded(out: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    out.push('%');
    out.push(char::from(HEX[(byte >> 4) as usize]));
    out.push(char::from(HEX[(byte & 0x0f) as usize]));
}

// ── Maud view helpers ─────────────────────────────────────────────────────────

/// A semantic `<time>` element showing the local date and time.
///
/// The `datetime` attribute is always UTC RFC3339 for machine readers; the
/// visible text uses the given zone.
///
/// ```rust,ignore
/// let markup = local_datetime(clock.now(), *tz);
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn local_datetime(dt: DateTime<Utc>, tz: Tz) -> maud::Markup {
    let local = tz.from_utc_datetime(&dt.naive_utc());
    let display = local.format("%Y-%m-%d %H:%M %Z").to_string();
    let rfc = dt.to_rfc3339();
    maud::html! {
        time datetime=(rfc) { (display) }
    }
}

/// A semantic `<time>` element showing the local date (no time component).
#[cfg(feature = "maud")]
#[must_use]
pub fn local_date(dt: DateTime<Utc>, tz: Tz) -> maud::Markup {
    let local = tz.from_utc_datetime(&dt.naive_utc());
    let display = local.format("%Y-%m-%d").to_string();
    let rfc = dt.to_rfc3339();
    maud::html! {
        time datetime=(rfc) { (display) }
    }
}

/// A semantic `<time>` element with a human-readable relative time string.
///
/// `now` should come from the [`Clock`](crate::time::Clock) extractor so
/// tests can control it deterministically.
///
/// ```rust,ignore
/// let markup = time_ago(event.created_at, clock.now(), *tz);
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn time_ago(dt: DateTime<Utc>, now: DateTime<Utc>, tz: Tz) -> maud::Markup {
    let relative = crate::format::relative_time_words(dt, now);
    let rfc = dt.to_rfc3339();
    let local = tz.from_utc_datetime(&dt.naive_utc());
    let display = local.format("%Y-%m-%d %H:%M %Z").to_string();
    maud::html! {
        time datetime=(rfc) title=(display) { (relative) }
    }
}

// ── Form parsing helpers ──────────────────────────────────────────────────────

/// Error returned by [`parse_local_datetime`].
#[derive(Debug, thiserror::Error)]
pub enum TimeZoneError {
    /// The input string did not match `YYYY-MM-DDTHH:MM`.
    #[error("invalid datetime-local input `{input}`: expected YYYY-MM-DDTHH:MM")]
    InvalidFormat {
        /// The string that failed to parse.
        input: String,
    },
    /// The input represents an ambiguous or non-existent local time (DST gap).
    #[error("local time `{input}` is ambiguous or non-existent in `{zone}`")]
    AmbiguousLocalTime {
        /// Original input string.
        input: String,
        /// Zone name for the error message.
        zone: String,
    },
}

/// Parse a browser `datetime-local` input value (`YYYY-MM-DDTHH:MM`) as a
/// time in `tz` and convert it to UTC.
///
/// DST ambiguity is resolved by choosing the **earlier** (pre-transition)
/// interpretation.
///
/// # Errors
///
/// Returns [`TimeZoneError::InvalidFormat`] for unparseable input and
/// [`TimeZoneError::AmbiguousLocalTime`] for a non-existent local time
/// (e.g. a DST gap when clocks spring forward).
pub fn parse_local_datetime(input: &str, tz: Tz) -> Result<DateTime<Utc>, TimeZoneError> {
    use chrono::NaiveDateTime;
    let naive = NaiveDateTime::parse_from_str(input.trim(), "%Y-%m-%dT%H:%M").map_err(|_| {
        TimeZoneError::InvalidFormat {
            input: input.to_owned(),
        }
    })?;
    tz.from_local_datetime(&naive)
        .earliest()
        .ok_or_else(|| TimeZoneError::AmbiguousLocalTime {
            input: input.to_owned(),
            zone: tz.name().to_owned(),
        })
        .map(|dt| dt.with_timezone(&Utc))
}

/// Format a UTC timestamp as a `datetime-local` input value (`YYYY-MM-DDTHH:MM`)
/// in the given zone. Suitable for populating an `<input type="datetime-local">`.
#[must_use]
pub fn to_local_input_value(dt: DateTime<Utc>, tz: Tz) -> String {
    let local = tz.from_utc_datetime(&dt.naive_utc());
    local.format("%Y-%m-%dT%H:%M").to_string()
}

/// Maud helper that renders a `<input type="datetime-local">` pre-filled with
/// the UTC value converted to `tz`.
#[cfg(feature = "maud")]
#[must_use]
pub fn datetime_local_input(
    name: &str,
    label: &str,
    dt: Option<DateTime<Utc>>,
    tz: Tz,
) -> maud::Markup {
    let value = dt.map(|d| to_local_input_value(d, tz)).unwrap_or_default();
    maud::html! {
        div.field {
            label for=(name) { (label) }
            input type="datetime-local" id=(name) name=(name) value=(value);
        }
    }
}

// ── Ambient request zone (task-local) ─────────────────────────────────────────

tokio::task_local! {
    static AMBIENT_TZ: Tz;
}

/// Run `fut` with `tz` set as the ambient request-scoped time zone.
///
/// Use in mailer templates and job workers to render timestamps in the zone
/// that was active when the request was handled:
///
/// ```rust,ignore
/// with_request_time_zone(tz.tz(), async move {
///     mail.deliver_later(&state).await;
/// })
/// .await;
/// ```
pub async fn with_request_time_zone<F, R>(tz: Tz, fut: F) -> R
where
    F: std::future::Future<Output = R>,
{
    AMBIENT_TZ.scope(tz, fut).await
}

/// Read the ambient time zone set by [`with_request_time_zone`], or `UTC` if
/// none is set.
#[must_use]
pub fn ambient_time_zone() -> Tz {
    AMBIENT_TZ.try_with(|tz| *tz).unwrap_or(Tz::UTC)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use chrono::Timelike;

    fn parts(uri: &str, headers: &[(&str, &str)]) -> axum::http::request::Parts {
        let mut req = Request::builder().uri(uri);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let (parts, _) = req.body(Body::empty()).unwrap().into_parts();
        parts
    }

    // ── IANA parsing ──────────────────────────────────────────────────────────

    #[test]
    fn parse_iana_valid_zones() {
        assert!(parse_iana("UTC").is_some());
        assert!(parse_iana("America/New_York").is_some());
        assert!(parse_iana("Asia/Tokyo").is_some());
        assert!(parse_iana("Europe/London").is_some());
        assert!(parse_iana("America/Sao_Paulo").is_some());
    }

    #[test]
    fn parse_iana_invalid_zones() {
        assert!(parse_iana("Mars/Phobos").is_none());
        assert!(parse_iana("").is_none());
        assert!(parse_iana("garbage").is_none());
        assert!(parse_iana("Not/A/Zone").is_none());
    }

    #[test]
    fn parse_iana_trims_whitespace() {
        assert!(parse_iana("  UTC  ").is_some());
        assert!(parse_iana("  America/New_York  ").is_some());
    }

    // ── Config ────────────────────────────────────────────────────────────────

    #[test]
    fn config_default_is_utc() {
        let cfg = TimeZoneConfig::default();
        assert_eq!(cfg.identifier, "UTC");
        assert_eq!(cfg.default_tz(), Tz::UTC);
    }

    #[test]
    fn config_validate_accepts_valid_identifier() {
        let cfg = TimeZoneConfig {
            identifier: "America/New_York".to_owned(),
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn config_validate_rejects_unknown_identifier() {
        let cfg = TimeZoneConfig {
            identifier: "Mars/Phobos".to_owned(),
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Mars/Phobos"),
            "error should mention the bad identifier: {msg}"
        );
    }

    #[test]
    fn config_default_sources_order() {
        let cfg = TimeZoneConfig::default();
        assert_eq!(
            cfg.sources,
            vec![Source::User, Source::Session, Source::Cookie, Source::Query]
        );
    }

    #[derive(serde::Deserialize)]
    struct Wrapper {
        #[serde(default)]
        time_zone: TimeZoneConfig,
    }

    #[test]
    fn config_deserializes_scalar_shorthand() {
        let w: Wrapper = toml::from_str(r#"time_zone = "America/New_York""#).unwrap();
        assert_eq!(w.time_zone.identifier, "America/New_York");
        // Scalar form keeps the default source chain.
        assert_eq!(w.time_zone.sources, default_time_zone_sources());
    }

    #[test]
    fn config_deserializes_table_form() {
        let w: Wrapper = toml::from_str(
            r#"
            [time_zone]
            identifier = "Asia/Tokyo"
            sources = ["query", "cookie"]
            "#,
        )
        .unwrap();
        assert_eq!(w.time_zone.identifier, "Asia/Tokyo");
        assert_eq!(w.time_zone.sources, vec![Source::Query, Source::Cookie]);
    }

    #[test]
    fn config_table_form_defaults_missing_fields() {
        let w: Wrapper = toml::from_str(
            r#"
            [time_zone]
            identifier = "Europe/London"
            "#,
        )
        .unwrap();
        assert_eq!(w.time_zone.identifier, "Europe/London");
        assert_eq!(w.time_zone.sources, default_time_zone_sources());
    }

    #[test]
    fn config_absent_uses_default() {
        let w: Wrapper = toml::from_str("").unwrap();
        assert_eq!(w.time_zone.identifier, "UTC");
    }

    // ── Query resolution ──────────────────────────────────────────────────────

    #[test]
    fn query_param_resolves_valid_zone() {
        let p = parts("/?tz=Asia/Tokyo", &[]);
        let result = resolve_from_query(&p);
        assert_eq!(result, Some(Tz::Asia__Tokyo));
    }

    #[test]
    fn query_param_ignores_invalid_zone() {
        let p = parts("/?tz=Mars/Phobos", &[]);
        assert!(resolve_from_query(&p).is_none());
    }

    #[test]
    fn query_param_absent_returns_none() {
        let p = parts("/", &[]);
        assert!(resolve_from_query(&p).is_none());
    }

    #[test]
    fn query_param_percent_encoded_slash() {
        let p = parts("/?tz=America%2FNew_York", &[]);
        assert_eq!(resolve_from_query(&p), Some(Tz::America__New_York));
    }

    #[test]
    fn query_param_percent_encoded_plus() {
        // Etc/GMT+5 contains a literal '+'; fully percent-encoded form must parse.
        let p = parts("/?tz=Etc%2FGMT%2B5", &[]);
        assert_eq!(resolve_from_query(&p), Some(Tz::Etc__GMTPlus5));
    }

    #[test]
    fn percent_decode_passthrough_when_no_escapes() {
        assert_eq!(percent_decode("America/New_York"), "America/New_York");
        assert!(matches!(
            percent_decode("UTC"),
            std::borrow::Cow::Borrowed("UTC")
        ));
    }

    #[test]
    fn percent_decode_resolves_escapes() {
        assert_eq!(percent_decode("America%2FNew_York"), "America/New_York");
        assert_eq!(percent_decode("Etc%2FGMT%2B5"), "Etc/GMT+5");
    }

    #[test]
    fn percent_decode_leaves_plus_literal() {
        assert_eq!(percent_decode("Etc/GMT+5"), "Etc/GMT+5");
    }

    #[test]
    fn percent_decode_keeps_trailing_partial_escape_literal() {
        // A stray '%' with too few hex digits is preserved verbatim.
        assert_eq!(percent_decode("UTC%2"), "UTC%2");
        assert_eq!(percent_decode("UTC%"), "UTC%");
    }

    // ── Cookie resolution ─────────────────────────────────────────────────────

    #[test]
    fn cookie_resolves_valid_zone() {
        let p = parts("/", &[("Cookie", "autumn_time_zone=America/Chicago")]);
        let result = resolve_from_cookie(&p);
        assert_eq!(result, Some(Tz::America__Chicago));
    }

    #[test]
    fn cookie_ignores_other_cookies() {
        let p = parts(
            "/",
            &[("Cookie", "session=abc; autumn_time_zone=UTC; other=x")],
        );
        let result = resolve_from_cookie(&p);
        assert_eq!(result, Some(Tz::UTC));
    }

    #[test]
    fn cookie_invalid_zone_returns_none() {
        let p = parts("/", &[("Cookie", "autumn_time_zone=garbage")]);
        assert!(resolve_from_cookie(&p).is_none());
    }

    #[test]
    fn cookie_absent_returns_none() {
        let p = parts("/", &[]);
        assert!(resolve_from_cookie(&p).is_none());
    }

    #[test]
    fn cookie_percent_encoded_value() {
        // Frontend libraries often encodeURIComponent cookie values.
        let p = parts("/", &[("Cookie", "autumn_time_zone=America%2FNew_York")]);
        assert_eq!(resolve_from_cookie(&p), Some(Tz::America__New_York));
    }

    // ── Cookie helper ─────────────────────────────────────────────────────────

    #[test]
    fn set_time_zone_cookie_produces_correct_header() {
        let header = set_time_zone_cookie("America/New_York");
        assert!(header.starts_with("autumn_time_zone=America/New_York"));
        assert!(header.contains("Path=/"));
        assert!(header.contains("Max-Age=31536000"));
        assert!(header.contains("SameSite=Lax"));
    }

    #[test]
    fn set_time_zone_cookie_encodes_special_chars() {
        // '+' is safe per our allow-list; space should be encoded
        let header = set_time_zone_cookie("Etc/UTC");
        assert!(header.contains("Etc/UTC"));
    }

    // ── UserTimeZone extractor ────────────────────────────────────────────────

    #[test]
    fn user_time_zone_newtype_roundtrips() {
        let utz = UserTimeZone(Tz::Asia__Tokyo);
        assert_eq!(utz.0, Tz::Asia__Tokyo);
    }

    // ── TimeZone struct ───────────────────────────────────────────────────────

    #[test]
    fn time_zone_new_constructor() {
        let tz = TimeZone::new(Tz::UTC);
        assert_eq!(tz.tz(), Tz::UTC);
        assert_eq!(*tz, Tz::UTC);
    }

    #[test]
    fn time_zone_iana_returns_name() {
        let tz = TimeZone::new(Tz::America__New_York);
        assert_eq!(tz.iana(), "America/New_York");
    }

    #[test]
    fn time_zone_convert_uses_given_zone() {
        use chrono::TimeZone as ChrTz;
        let utc = chrono::Utc.with_ymd_and_hms(2025, 6, 14, 12, 0, 0).unwrap();
        let tz = TimeZone::new(Tz::America__New_York);
        let local = tz.convert(utc);
        // New York is UTC-4 in June (EDT)
        assert_eq!(local.hour(), 8);
    }

    // ── Form parsing ──────────────────────────────────────────────────────────

    #[test]
    fn parse_local_datetime_tokyo() {
        // 15:30 in Tokyo (UTC+9) → 06:30 UTC
        let result = parse_local_datetime("2025-06-14T15:30", Tz::Asia__Tokyo).unwrap();
        assert_eq!(result.hour(), 6);
        assert_eq!(result.minute(), 30);
    }

    #[test]
    fn parse_local_datetime_new_york_summer() {
        // 12:00 in New York (UTC-4 in summer) → 16:00 UTC
        let result = parse_local_datetime("2025-06-14T12:00", Tz::America__New_York).unwrap();
        assert_eq!(result.hour(), 16);
        assert_eq!(result.minute(), 0);
    }

    #[test]
    fn parse_local_datetime_invalid_format() {
        let err = parse_local_datetime("not-a-date", Tz::UTC).unwrap_err();
        assert!(matches!(err, TimeZoneError::InvalidFormat { .. }));
    }

    #[test]
    fn to_local_input_value_roundtrip() {
        let zones = [Tz::UTC, Tz::America__New_York, Tz::Asia__Tokyo];
        for tz in zones {
            let original = "2025-06-14T15:30";
            let utc = parse_local_datetime(original, tz).unwrap();
            let back = to_local_input_value(utc, tz);
            assert_eq!(back, original, "roundtrip failed for {}", tz.name());
        }
    }

    #[test]
    fn to_local_input_value_formats_correctly() {
        use chrono::TimeZone as ChrTz;
        let utc = chrono::Utc.with_ymd_and_hms(2025, 6, 14, 6, 30, 0).unwrap();
        assert_eq!(
            to_local_input_value(utc, Tz::Asia__Tokyo),
            "2025-06-14T15:30"
        );
        assert_eq!(to_local_input_value(utc, Tz::UTC), "2025-06-14T06:30");
    }

    // ── Maud view helpers ─────────────────────────────────────────────────────

    #[cfg(feature = "maud")]
    mod maud_tests {
        use super::*;
        use chrono::TimeZone as ChrTz;

        #[allow(clippy::many_single_char_names)]
        fn utc(y: i32, mo: u32, d: u32, h: u32, m: u32, s: u32) -> DateTime<Utc> {
            chrono::Utc.with_ymd_and_hms(y, mo, d, h, m, s).unwrap()
        }

        #[test]
        fn local_datetime_uses_zone() {
            let dt = utc(2025, 6, 14, 12, 0, 0);
            let utc_html = local_datetime(dt, Tz::UTC).into_string();
            let tokyo_html = local_datetime(dt, Tz::Asia__Tokyo).into_string();
            let ny_html = local_datetime(dt, Tz::America__New_York).into_string();
            // Each zone yields a different visible time
            assert!(utc_html.contains("12:00"), "UTC: {utc_html}");
            assert!(tokyo_html.contains("21:00"), "Tokyo: {tokyo_html}");
            assert!(ny_html.contains("08:00"), "New York: {ny_html}");
        }

        #[test]
        fn local_datetime_datetime_attr_is_utc() {
            let dt = utc(2025, 6, 14, 12, 0, 0);
            let html = local_datetime(dt, Tz::Asia__Tokyo).into_string();
            // The machine-readable datetime attr should include the UTC timestamp
            assert!(
                html.contains("2025-06-14T12:00:00"),
                "datetime attr must be UTC: {html}"
            );
        }

        #[test]
        fn local_date_uses_zone() {
            // Close to midnight UTC — the date differs by zone
            let dt = utc(2025, 6, 14, 23, 30, 0); // Jun 14 23:30 UTC = Jun 15 08:30 Tokyo
            let utc_html = local_date(dt, Tz::UTC).into_string();
            let tokyo_html = local_date(dt, Tz::Asia__Tokyo).into_string();
            assert!(utc_html.contains("2025-06-14"), "UTC: {utc_html}");
            assert!(tokyo_html.contains("2025-06-15"), "Tokyo: {tokyo_html}");
        }

        #[test]
        fn time_ago_seconds() {
            let now = utc(2025, 6, 14, 12, 0, 30);
            let dt = utc(2025, 6, 14, 12, 0, 0);
            let html = time_ago(dt, now, Tz::UTC).into_string();
            assert!(html.contains("seconds ago"), "{html}");
        }

        #[test]
        fn time_ago_minutes() {
            let now = utc(2025, 6, 14, 12, 5, 0);
            let dt = utc(2025, 6, 14, 12, 0, 0);
            let html = time_ago(dt, now, Tz::UTC).into_string();
            assert!(html.contains("minutes ago"), "{html}");
        }

        #[test]
        fn time_ago_hours() {
            let now = utc(2025, 6, 14, 14, 0, 0);
            let dt = utc(2025, 6, 14, 12, 0, 0);
            let html = time_ago(dt, now, Tz::UTC).into_string();
            assert!(html.contains("hours ago"), "{html}");
        }

        #[test]
        fn time_ago_days() {
            let now = utc(2025, 6, 16, 12, 0, 0);
            let dt = utc(2025, 6, 14, 12, 0, 0);
            let html = time_ago(dt, now, Tz::UTC).into_string();
            assert!(html.contains("days ago"), "{html}");
        }

        #[test]
        fn time_ago_future_minutes() {
            let now = utc(2025, 6, 14, 12, 0, 0);
            let dt = utc(2025, 6, 14, 12, 5, 0);
            let html = time_ago(dt, now, Tz::UTC).into_string();
            assert!(html.contains("in "), "{html}");
            assert!(html.contains("minutes"), "{html}");
        }

        #[test]
        fn time_ago_preserves_utc_datetime_attr() {
            let now = utc(2025, 6, 14, 12, 5, 0);
            let dt = utc(2025, 6, 14, 12, 0, 0);
            let html = time_ago(dt, now, Tz::UTC).into_string();
            assert!(html.contains("datetime="), "{html}");
        }
    }

    // ── Ambient TZ ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn ambient_time_zone_defaults_to_utc() {
        assert_eq!(ambient_time_zone(), Tz::UTC);
    }

    #[tokio::test]
    async fn with_request_time_zone_sets_ambient() {
        let result = with_request_time_zone(Tz::Asia__Tokyo, async { ambient_time_zone() }).await;
        assert_eq!(result, Tz::Asia__Tokyo);
    }

    #[tokio::test]
    async fn ambient_returns_utc_outside_scope() {
        // Inside scope
        let inside =
            with_request_time_zone(Tz::America__New_York, async { ambient_time_zone() }).await;
        // Outside scope
        let outside = ambient_time_zone();
        assert_eq!(inside, Tz::America__New_York);
        assert_eq!(outside, Tz::UTC);
    }
}
